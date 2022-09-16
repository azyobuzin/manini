use aws_sdk_ecs as ecs;
use futures::future::pending;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};
use tokio::time::sleep;

#[derive(Clone, Debug)]
pub struct ServiceScaler {
    target_addr_rx: watch::Receiver<Option<IpAddr>>,
    inner: Arc<ServiceScalerInner>,
}

#[derive(Debug, Default)]
struct ServiceScalerInner {
    pending: AtomicBool,
    notify: Notify,
}

impl ServiceScaler {
    pub async fn target_addr(&mut self) -> IpAddr {
        loop {
            if let Some(x) = self.target_addr_rx.borrow_and_update().as_ref() {
                return x.to_owned();
            }

            // make a request to launch the container
            if !self.inner.pending.swap(true, Ordering::SeqCst) {
                self.inner.notify.notify_one();
            }

            if let Err(_) = self.target_addr_rx.changed().await {
                // if the sender is dropped, this task never completes
                pending().await
            }
        }
    }
}

pub fn run_scaler(
    ecs_client: ecs::Client,
    cluster_name: String,
    service_name: String,
) -> ServiceScaler {
    let (target_addr_tx, target_addr_rx) = watch::channel(None);
    let inner: Arc<ServiceScalerInner> = Default::default();
    let result = ServiceScaler {
        target_addr_rx,
        inner: inner.clone(),
    };

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = inner.notify.notified() => {
                    if inner.pending.load(Ordering::SeqCst) {
                        // TODO: スケーリングし、ECSがhealthyになるまで待つ
                    }
                }
                _ = sleep(Duration::from_secs(30)) => {
                    // nothing happened within 30 secs
                    // check ECS status
                }
                _ = target_addr_tx.closed() => return,
            }
        }
    });

    return result;
}
