use crate::{RequestGuard, ServiceScaler};
use http::header::{ACCEPT, CONTENT_TYPE, HOST, RETRY_AFTER, UPGRADE};
use http::uri::Scheme;
use http::{HeaderValue, Method, StatusCode, Uri, Version};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Body as HyperBody, Bytes, Frame, Incoming, SizeHint};
use hyper::{Request, Response};
use hyper_util::client::legacy::connect::Connect;
use hyper_util::client::legacy::{Client, Error as ClientError};
use hyper_util::rt::TokioIo;
use log::{debug, error, info, warn};
use memchr::memmem;
use pin_project_lite::pin_project;
use std::error::Error;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::error::Elapsed;
use tokio::time::timeout;

pub type ProxyHttpBody = BoxBody<Bytes, hyper::Error>;

#[derive(Debug, Clone)]
pub struct ProxyServiceOptions<C> {
    pub service_scaler: ServiceScaler,
    pub target_port: u16,
    pub http_client: Client<C, ProxyHttpBody>,
    pub remote_addr: IpAddr,
}

pin_project! {
    #[derive(Debug)]
    pub struct WrappedBody {
        #[pin]
        inner: ProxyHttpBody,
        // Drop the guard after sending response
        _request_guard: Option<RequestGuard>,
    }
}

impl HyperBody for WrappedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.project().inner.poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

pub async fn proxy_service_fn<C>(
    mut req: Request<Incoming>,
    options: &ProxyServiceOptions<C>,
) -> anyhow::Result<Response<WrappedBody>>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    if !is_supported_method(req.method()) {
        // Return 405 for unsupported metho
        info!("Unsupported request {} {}", req.method(), req.uri().path());
        let res = Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(empty_body())?;
        return Ok(wrap_response(res, None));
    }

    let request_guard = {
        let guard_fut = options.service_scaler.handle_request();
        if is_get_html_request(&req) {
            let timeout_result = timeout(Duration::from_secs(5), guard_fut).await;
            match timeout_result {
                Ok(x) => x?,
                Err(_) => {
                    // Return the waiting message for browsers
                    debug!("Return waiting message");
                    let res = Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header(RETRY_AFTER, "10")
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(full_body(WAITING_RESPONSE))?;
                    return Ok(wrap_response(res, None));
                }
            }
        } else {
            guard_fut.await?
        }
    };

    // Set Host header
    if !req.headers().contains_key(HOST) {
        if let Some(x) = req.uri().host() {
            let hv: HeaderValue = x.parse()?;
            req.headers_mut().insert(HOST, hv);
        }
    }

    // Set X-Forwarded-For header
    req.headers_mut()
        .append("X-Forwarded-For", options.remote_addr.to_string().parse()?);

    // Rewrite URI
    let path_and_query = req.uri().path_and_query();
    *req.uri_mut() = {
        let builder = Uri::builder().scheme(Scheme::HTTP).authority(
            SocketAddr::new(request_guard.target_addr(), options.target_port).to_string(),
        );
        let builder = match path_and_query {
            Some(x) => builder.path_and_query(x.clone()),
            None => builder,
        };
        builder.build()?
    };

    if !req.headers().contains_key(UPGRADE) {
        let request_method = req.method().clone();
        let request_uri = req.uri().clone();
        debug!("Send request {} {}", request_method, request_uri);

        let forward_req = req.map(|body| body.boxed());
        let response = match options.http_client.request(forward_req).await {
            Ok(res) => wrap_client_response(res, Some(request_guard)),
            Err(e) => {
                error!("Error on request {} {}: {}", request_method, request_uri, e);
                wrap_response(bad_gateway_response(&e), Some(request_guard))
            }
        };

        return Ok(response);
    }

    // The request is an upgrade (WebSocket) request
    debug!("Send upgrade request {} {}", req.method(), req.uri());

    // Copy the request
    let mut req_to_send = Request::builder()
        .method(req.method())
        .uri(req.uri())
        .version(Version::HTTP_11);
    {
        let headers = req_to_send.headers_mut().unwrap();
        headers.clear();
        req.headers().iter().for_each(|(k, v)| {
            headers.append(k, v.clone());
        });
    }
    let req_to_send = req_to_send.body(empty_body())?;

    let mut res = match options.http_client.request(req_to_send).await {
        Ok(x) => x,
        Err(e) => {
            error!("Error on request {} {}: {}", req.method(), req.uri(), e);
            return Ok(wrap_response(bad_gateway_response(&e), Some(request_guard)));
        }
    };

    if res.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Ok(wrap_client_response(res, Some(request_guard)));
    }

    // Connect the streams
    let res_upgraded = hyper::upgrade::on(&mut res).await?;
    tokio::spawn(async move {
        let req_path = req.uri().path().to_owned();
        let req_upgraded = match hyper::upgrade::on(req).await {
            Ok(x) => x,
            Err(e) => {
                error!("Failed to upgrade request to {}: {}", req_path, e);
                return;
            }
        };

        let mut res_io = TokioIo::new(res_upgraded);
        let mut req_io = TokioIo::new(req_upgraded);

        if let Err(e) = tokio::io::copy_bidirectional(&mut res_io, &mut req_io).await {
            warn!("Error on upgraded connection of {}: {}", req_path, e);
        }

        // If the connection upgraded, drop the guard after disconnection.
        drop(request_guard);
    });

    Ok(wrap_client_response(res, None))
}

fn is_supported_method(method: &Method) -> bool {
    // Deny CONNECT and TRACE
    method == Method::GET
        || method == Method::POST
        || method == Method::PUT
        || method == Method::DELETE
        || method == Method::HEAD
        || method == Method::OPTIONS
        || method == Method::PATCH
}

fn is_get_html_request<B>(req: &Request<B>) -> bool {
    req.method() == Method::GET
        && !req.headers().contains_key(UPGRADE)
        && req
            .headers()
            .get_all(ACCEPT)
            .iter()
            .any(|v| memmem::find(&v.as_bytes().to_ascii_lowercase(), b"text/html").is_some())
}

fn wrap_client_response(
    res: Response<Incoming>,
    request_guard: Option<RequestGuard>,
) -> Response<WrappedBody> {
    let res = res.map(|body| body.boxed());
    wrap_response(res, request_guard)
}

fn wrap_response(
    res: Response<ProxyHttpBody>,
    request_guard: Option<RequestGuard>,
) -> Response<WrappedBody> {
    let (parts, body) = res.into_parts();
    Response::from_parts(
        parts,
        WrappedBody {
            inner: body,
            _request_guard: request_guard,
        },
    )
}

fn empty_body() -> ProxyHttpBody {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

fn full_body(bytes: &'static [u8]) -> ProxyHttpBody {
    Full::new(Bytes::from_static(bytes))
        .map_err(|never| match never {})
        .boxed()
}

fn bad_gateway_response(err: &ClientError) -> Response<ProxyHttpBody> {
    let status = if is_timeout_error(err) {
        StatusCode::GATEWAY_TIMEOUT
    } else {
        StatusCode::BAD_GATEWAY
    };

    Response::builder()
        .status(status)
        .body(empty_body())
        .unwrap()
}

fn is_timeout_error(err: &ClientError) -> bool {
    let mut source = err.source();
    while let Some(src) = source {
        if src.is::<Elapsed>() {
            return true;
        }
        if let Some(io_err) = src.downcast_ref::<io::Error>() {
            if io_err.kind() == io::ErrorKind::TimedOut {
                return true;
            }
        }
        source = src.source();
    }
    false
}

const WAITING_RESPONSE: &'static [u8] = include_bytes!("waiting.html");
