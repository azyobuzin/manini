use anyhow::bail;
use http::header::{ACCEPT, CONTENT_TYPE, HOST, RETRY_AFTER, UPGRADE};
use http::uri::Scheme;
use http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, Version};
use hyper::body::{HttpBody, SizeHint};
use hyper::client::connect::Connect;
use hyper::{Body, Request, Response};
use log::{debug, error, info, warn};
use memchr::memmem;
use std::convert::TryFrom;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::timeout;

use crate::{RequestGuard, ServiceScaler};

#[derive(Debug, Clone)]
pub struct ProxyServiceOptions<C> {
    pub service_scaler: ServiceScaler,
    pub target_port: u16,
    pub http_client: hyper::Client<C>,
    pub remote_addr: IpAddr,
}

#[derive(Debug)]
pub struct WrappedBody {
    inner: Body,
    // Drop the guard after sending response
    _request_guard: Option<RequestGuard>,
}

impl HttpBody for WrappedBody {
    type Data = <Body as HttpBody>::Data;
    type Error = <Body as HttpBody>::Error;

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        Pin::new(&mut self.inner).poll_data(cx)
    }

    fn poll_trailers(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
    ) -> Poll<Result<Option<HeaderMap<HeaderValue>>, Self::Error>> {
        Pin::new(&mut self.inner).poll_trailers(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

pub async fn proxy_service_fn<C>(
    mut req: Request<Body>,
    options: &ProxyServiceOptions<C>,
) -> anyhow::Result<Response<WrappedBody>>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    if !is_supported_method(req.method()) {
        // Return 405 for unsupported method
        info!("Unsupported request {} {}", req.method(), req.uri().path());
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(wrap_body(Body::empty()))?);
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
                    return Ok(Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header(RETRY_AFTER, "10")
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(wrap_body(WAITING_RESPONSE.into()))?);
                }
            }
        } else {
            guard_fut.await?
        }
    };

    // Set Host header
    if !req.headers().contains_key(HOST) {
        if let Some(x) = req.uri().host() {
            match HeaderValue::try_from(x) {
                Ok(hv) => req.headers_mut().insert(HOST, hv),
                Err(e) => bail!(e),
            };
        }
    }

    // Set X-Forwarded-For header
    match HeaderValue::try_from(options.remote_addr.to_string()) {
        Ok(hv) => req.headers_mut().append("X-Forwarded-For", hv),
        Err(e) => bail!(e),
    };

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
        match builder.build() {
            Ok(x) => x,
            Err(e) => bail!(e),
        }
    };

    if !req.headers().contains_key(UPGRADE) {
        debug!("Send request {} {}", req.method(), req.uri());
        return Ok(wrap_response(
            options.http_client.request(req).await?,
            Some(request_guard),
        ));
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
    let req_to_send = req_to_send.body(Body::empty())?;

    let mut res = options.http_client.request(req_to_send).await?;

    if res.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Ok(wrap_response(res, Some(request_guard)));
    }

    // Connect the streams
    let mut res_upgraded = hyper::upgrade::on(&mut res).await?;
    tokio::spawn(async move {
        let req_path = req.uri().path().to_owned();
        let mut req_upgraded = match hyper::upgrade::on(req).await {
            Ok(x) => x,
            Err(e) => {
                error!("Failed to upgrade request to {}: {}", req_path, e);
                return;
            }
        };

        if let Err(e) = tokio::io::copy_bidirectional(&mut res_upgraded, &mut req_upgraded).await {
            warn!("Error on upgraded connection of {}: {}", req_path, e);
        }

        // If the connection upgraded, drop the guard after disconnection.
        drop(request_guard)
    });

    Ok(wrap_response(res, None))
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

fn wrap_body(body: Body) -> WrappedBody {
    WrappedBody {
        inner: body,
        _request_guard: None,
    }
}

fn wrap_response(
    res: Response<Body>,
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

const WAITING_RESPONSE: &'static [u8] = include_bytes!("waiting.html");
