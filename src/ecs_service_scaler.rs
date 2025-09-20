use anyhow::{bail, Context as _};
use aws_sdk_ec2 as ec2;
use aws_sdk_ecs as ecs;
use ecs::types::{DesiredStatus, HealthStatus};
use futures::TryFutureExt;
use log::{debug, error, info, warn};
use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, sleep_until, Instant};
use tokio::{select, spawn};

#[derive(Clone, Debug)]
pub struct ServiceScalerOptions {
    pub ecs_client: ecs::Client,
    pub ec2_client: ec2::Client,
    pub cluster_name: Option<String>,
    pub service_name: String,
    pub use_public_ip: bool,
    pub scale_down_period: Duration,
}

#[derive(Clone, Debug)]
pub struct ServiceScaler {
    service_status_rx: watch::Receiver<ServiceStatus>,
    notify_tx: mpsc::UnboundedSender<RequestNotification>,
}

#[derive(Debug)]
pub struct RequestGuard {
    addr: IpAddr,
    _notify_tx: oneshot::Sender<Infallible>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceStatus {
    Healthy(IpAddr),
    NoIP,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RequestNotification {
    ReceiveRequest,
    SentResponse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskStatus {
    Healthy,
    Unhealthy,
    NoTask,
}

impl ServiceScaler {
    pub fn handle_request(
        &self,
    ) -> impl Future<Output = Result<RequestGuard, anyhow::Error>> + 'static {
        let notify_tx = self.notify_tx.clone();
        let mut rx = self.service_status_rx.clone();

        async move {
            if let Err(_) = notify_tx.send(RequestNotification::ReceiveRequest) {
                bail!("The worker is dead")
            }

            let (oneshot_notify_tx, oneshot_notify_rx) = oneshot::channel();

            spawn(async move {
                // When oneshot_notify_tx is dropped, the request has completed.
                oneshot_notify_rx.await.ok();
                notify_tx.send(RequestNotification::SentResponse).ok();
            });

            loop {
                if let ServiceStatus::Healthy(addr) = *rx.borrow_and_update() {
                    return Ok(RequestGuard {
                        addr,
                        _notify_tx: oneshot_notify_tx,
                    });
                }

                rx.changed().await.context("The worker is dead")?;
            }
        }
    }
}

impl RequestGuard {
    pub fn target_addr(&self) -> IpAddr {
        self.addr
    }
}

pub fn run_scaler(options: ServiceScalerOptions) -> ServiceScaler {
    let (service_status_tx, service_status_rx) = watch::channel(ServiceStatus::Unhealthy);
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel();
    let result = ServiceScaler {
        service_status_rx,
        notify_tx,
    };

    spawn(async move {
        let mut alive_connections = 0isize;
        let mut next_health_check_time = Instant::now();
        let mut scale_down_time = None;
        const HEALTH_CHECK_PERIOD: Duration = Duration::from_secs(30);

        loop {
            enum TimeoutAction {
                HealthCheck,
                ScaleDown,
            }
            let (timeout_action, target_time) = match scale_down_time {
                Some(x) if x <= next_health_check_time => (TimeoutAction::ScaleDown, x),
                _ => (TimeoutAction::HealthCheck, next_health_check_time),
            };

            select! {
                msg = notify_rx.recv() => {
                    match msg {
                        Some(RequestNotification::ReceiveRequest) => alive_connections += 1,
                        Some(RequestNotification::SentResponse) => alive_connections -= 1,
                        None => return,
                    }

                    debug!("alive_connections = {}", alive_connections);

                    if alive_connections <= 0 {
                        assert_eq!(alive_connections, 0);
                        scale_down_time = Some(Instant::now() + options.scale_down_period);
                    } else {
                        scale_down_time = None;

                        if *service_status_tx.borrow() == ServiceStatus::Unhealthy {
                            let scale_result = scale_up_service(&options)
                                .and_then(|()| wait_until_healthy(&options))
                                .await;

                            if let Err(x) = scale_result {
                                error!("Failed to scale up the service: {}", x);
                            } else {
                                match get_service_ip(&options).await {
                                    Ok(x) => send_new_status(
                                        &service_status_tx,
                                        &match x {
                                            Some(addr) => ServiceStatus::Healthy(addr),
                                            None => ServiceStatus::NoIP
                                        }
                                    ),
                                    Err(x) => error!("Failed to get IP address: {}", x),
                                }
                            }

                            next_health_check_time = Instant::now() + HEALTH_CHECK_PERIOD;
                        }
                    }
                }

                _ = sleep_until(target_time) => {
                    match timeout_action {
                        TimeoutAction::HealthCheck => {
                            // Check ECS status if nothing happened within 30 secs
                            debug!("Starting health check");

                            let result = match get_task_status(&options).await {
                                Ok(TaskStatus::Healthy) => match get_service_ip(&options).await {
                                    Ok(Some(addr)) => Ok(ServiceStatus::Healthy(addr)),
                                    Ok(None) => Ok(ServiceStatus::NoIP),
                                    Err(x) => Err(x)
                                }
                                Ok(_) => Ok(ServiceStatus::Unhealthy),
                                Err(x) => Err(x)
                            };

                            match &result {
                                Ok(status) => send_new_status(&service_status_tx, status),
                                Err(x) => error!("Health check error: {}", x),
                            }

                            if let Ok(ServiceStatus::Healthy(_)) = result {
                                if alive_connections <= 0 && scale_down_time.is_none() {
                                    scale_down_time = Some(Instant::now() + options.scale_down_period);
                                }
                            }
                        }
                        TimeoutAction::ScaleDown => {
                            // Scale down the service if no request within the specified seconds
                            info!("Scaling down the service due to no request is received");
                            scale_down_time = None;

                            if let Err(_) = service_status_tx.send(ServiceStatus::Unhealthy) {
                                return // The channel is closed
                            }

                            if let Err(x) = scale_down_service(&options).await {
                                error!("Failed to scale down the service: {}", x);
                            }
                        }
                    }

                    next_health_check_time = Instant::now() + HEALTH_CHECK_PERIOD;
                }

                _ = service_status_tx.closed() => return,
            }
        }
    });

    return result;
}

async fn scale_up_service(options: &ServiceScalerOptions) -> anyhow::Result<()> {
    let desired_count = {
        let services_res = options
            .ecs_client
            .describe_services()
            .set_cluster(options.cluster_name.clone())
            .services(&options.service_name)
            .send()
            .await
            .context("Failed to get service status")?;
        let service = match services_res.services() {
            [ref x] => x,
            [] => bail!("The specified service is not found"),
            _ => bail!("Multiple services are found"),
        };
        service.desired_count()
    };

    if desired_count < 1 {
        info!("Scaling up");
        options
            .ecs_client
            .update_service()
            .set_cluster(options.cluster_name.clone())
            .service(&options.service_name)
            .desired_count(1)
            .send()
            .await
            .context("Failed to update desired count")?;
    }

    Ok(())
}

async fn wait_until_healthy(options: &ServiceScalerOptions) -> anyhow::Result<()> {
    loop {
        match get_task_status(options).await {
            Ok(TaskStatus::Healthy) => return Ok(()),
            Ok(TaskStatus::Unhealthy) => info!("Waiting until healthy: The task is not healthy"),
            Ok(TaskStatus::NoTask) => info!("Waiting until healthy: No task is running"),
            Err(x) => return Err(x),
        }

        sleep(Duration::from_secs(5)).await;
    }
}

async fn get_task_status(options: &ServiceScalerOptions) -> anyhow::Result<TaskStatus> {
    let task_arns: Vec<String> = {
        let list_tasks_res = options
            .ecs_client
            .list_tasks()
            .set_cluster(options.cluster_name.clone())
            .service_name(&options.service_name)
            .desired_status(DesiredStatus::Running)
            .send()
            .await?;
        match list_tasks_res.task_arns {
            Some(x) if !x.is_empty() => x,
            _ => return Ok(TaskStatus::NoTask),
        }
    };

    let tasks_res = options
        .ecs_client
        .describe_tasks()
        .set_cluster(options.cluster_name.clone())
        .set_tasks(Some(task_arns))
        .send()
        .await?;

    if tasks_res
        .tasks()
        .iter()
        .any(|task| matches!(task.health_status(), Some(HealthStatus::Healthy)))
    {
        Ok(TaskStatus::Healthy)
    } else {
        Ok(TaskStatus::Unhealthy)
    }
}

async fn get_service_ip(options: &ServiceScalerOptions) -> anyhow::Result<Option<IpAddr>> {
    let task_arns: Vec<String> = {
        let list_tasks_res = options
            .ecs_client
            .list_tasks()
            .set_cluster(options.cluster_name.clone())
            .service_name(&options.service_name)
            .desired_status(DesiredStatus::Running)
            .send()
            .await?;
        match list_tasks_res.task_arns {
            Some(x) if !x.is_empty() => x,
            _ => return Ok(None),
        }
    };

    let tasks_res = options
        .ecs_client
        .describe_tasks()
        .set_cluster(options.cluster_name.clone())
        .set_tasks(Some(task_arns))
        .send()
        .await?;

    // Get the ENI attached to the task
    let attachment = match tasks_res
        .tasks()
        .iter()
        .flat_map(|task| task.attachments().iter())
        .find(|attachment| {
            matches!(
                (attachment.r#type(), attachment.status()),
                (Some("ElasticNetworkInterface"), Some("ATTACHED"))
            )
        }) {
        Some(x) => x,
        None => return Ok(None),
    };

    if options.use_public_ip {
        let eni_id = attachment
            .details()
            .iter()
            .find_map(|x| match (x.name(), x.value()) {
                (Some("networkInterfaceId"), Some(id)) => Some(id),
                _ => None,
            })
            .context("Unable to get networkInterfaceId")?;
        let enis_res = options
            .ec2_client
            .describe_network_interfaces()
            .network_interface_ids(eni_id)
            .send()
            .await?;
        let public_ip = enis_res
            .network_interfaces()
            .iter()
            .flat_map(|x| x.association().into_iter())
            .flat_map(|x| x.public_ip().into_iter())
            .nth(0)
            .context("No public IP is attached")?;
        return Ok(Some(IpAddr::from_str(public_ip)?));
    }

    let private_ip = attachment
        .details()
        .iter()
        .find_map(|x| match (x.name(), x.value()) {
            (Some("privateIPv4Address"), Some(private_ip)) => Some(private_ip),
            _ => None,
        })
        .context("Unable to get privateIPv4Address")?;
    Ok(Some(IpAddr::from_str(private_ip)?))
}

fn send_new_status(tx: &watch::Sender<ServiceStatus>, status: &ServiceStatus) {
    let modified = tx.send_if_modified(|v| {
        if v != status {
            *v = status.clone();
            return true;
        }
        false
    });

    if modified {
        match status {
            ServiceStatus::Healthy(new_ip) => info!("Service IP address: {}", new_ip),
            ServiceStatus::NoIP => warn!("The service is healthy but no IP address is attached"),
            ServiceStatus::Unhealthy => info!("The service is unhealthy"),
        }
    }
}

async fn scale_down_service(options: &ServiceScalerOptions) -> anyhow::Result<()> {
    options
        .ecs_client
        .update_service()
        .set_cluster(options.cluster_name.clone())
        .service(&options.service_name)
        .desired_count(0)
        .send()
        .await?;
    Ok(())
}
