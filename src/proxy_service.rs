use futures::future::BoxFuture;
use futures::TryFutureExt;
use http::header::HOST;
use http::uri::Scheme;
use http::{HeaderValue, Uri};
use hyper::client::connect::Connect;
use hyper::service::Service;
use hyper::{Body, Request, Response};
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

        if !req.headers().contains_key(HOST) {
            if let Some(x) = req.uri().host() {
                match HeaderValue::try_from(x) {
                    Ok(hv) => req.headers_mut().insert(HOST, hv),
                    Err(e) => return Box::pin(ready(Err(e.into()))),
                };
            }
        }

        match HeaderValue::try_from(self.remote_addr.to_string()) {
            Ok(hv) => req.headers_mut().append("X-Forwarded-For", hv),
            Err(e) => return Box::pin(ready(Err(e.into()))),
        };

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

        Box::pin(self.http_client.request(req).map_err(anyhow::Error::from))

        // TODO: websocket support
    }
}
