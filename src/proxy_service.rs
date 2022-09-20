use futures::future::BoxFuture;
use futures::TryFutureExt;
use http::header::{HOST, UPGRADE};
use http::uri::Scheme;
use http::{HeaderValue, Method, StatusCode, Uri, Version};
use hyper::client::connect::Connect;
use hyper::service::Service;
use hyper::{Body, Request, Response};
use log::{error, warn};
use std::convert::TryFrom;
use std::future::ready;
use std::net::{IpAddr, SocketAddr};
use std::task::{Context, Poll};

pub struct ProxyService<C> {
    service_scaler: super::ServiceScaler,
    target_port: u16,
    target_addr: Option<IpAddr>,
    http_client: hyper::Client<C>,
    remote_addr: IpAddr,
}

impl<C> ProxyService<C> {
    pub fn new(
        service_scaler: super::ServiceScaler,
        target_port: u16,
        http_client: hyper::Client<C>,
        remote_addr: IpAddr,
    ) -> Self {
        ProxyService {
            service_scaler,
            target_port,
            target_addr: None,
            http_client,
            remote_addr,
        }
    }
}

impl<C> Service<Request<Body>> for ProxyService<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    type Response = Response<Body>;
    type Error = anyhow::Error;
    type Future = BoxFuture<'static, anyhow::Result<Self::Response>>;

    fn poll_ready(&mut self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        match self.service_scaler.poll_target_addr(cx) {
            Poll::Ready(addr) => {
                self.target_addr = Some(addr);
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                self.target_addr = None;
                Poll::Pending
            }
        }
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        let target_addr = SocketAddr::new(
            self.target_addr.expect("not ready to call"),
            self.target_port,
        );

        if !is_supported_method(req.method()) {
            // Return 405 for unsupported method
            return Box::pin(ready(Ok(Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .body(Body::empty())
                .unwrap())));
        }

        // Set Host header
        if !req.headers().contains_key(HOST) {
            if let Some(x) = req.uri().host() {
                match HeaderValue::try_from(x) {
                    Ok(hv) => req.headers_mut().insert(HOST, hv),
                    Err(e) => return Box::pin(ready(Err(e.into()))),
                };
            }
        }

        // Set X-Forwarded-For header
        match HeaderValue::try_from(self.remote_addr.to_string()) {
            Ok(hv) => req.headers_mut().append("X-Forwarded-For", hv),
            Err(e) => return Box::pin(ready(Err(e.into()))),
        };

        // Rewrite URI
        let path_and_query = req.uri().path_and_query();
        *req.uri_mut() = {
            let builder = Uri::builder()
                .scheme(Scheme::HTTP)
                .authority(target_addr.to_string());
            let builder = match path_and_query {
                Some(x) => builder.path_and_query(x.clone()),
                None => builder,
            };
            match builder.build() {
                Ok(x) => x,
                Err(e) => return Box::pin(ready(Err(e.into()))),
            }
        };

        if !req.headers().contains_key(UPGRADE) {
            return Box::pin(self.http_client.request(req).map_err(anyhow::Error::from));
        }

        // The request is an upgrade (WebSocket) request
        let http_client = self.http_client.clone();
        return Box::pin(async move {
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

            let mut res = http_client.request(req_to_send).await?;

            if res.status() != StatusCode::SWITCHING_PROTOCOLS {
                return Ok(res);
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

                if let Err(e) =
                    tokio::io::copy_bidirectional(&mut res_upgraded, &mut req_upgraded).await
                {
                    warn!("Error on upgraded connection of {}: {}", req_path, e);
                }
            });

            Ok(res)
        });
    }
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
