use anyhow::Context as _;
use aws_sdk_ec2 as ec2;
use aws_sdk_ecs as ecs;
use ecs::model::{DesiredStatus, HealthStatus};
use futures::{Future, TryFutureExt};
use log::{debug, error, info, warn};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::watch::Sender;
use tokio::sync::{watch, Notify};
use tokio::time::{sleep, sleep_until, Instant};
use tokio::{pin, select, spawn};

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
    request_notify: Arc<Notify>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceStatus {
    Healthy(IpAddr),
    NoIP,
    Unhealthy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskStatus {
    Healthy,
    Unhealthy,
    NoTask,
}

impl ServiceScaler {
    pub fn poll_target_addr(&mut self, cx: &mut Context) -> Poll<IpAddr> {
        self.request_notify.notify_one();

        loop {
            if let ServiceStatus::Healthy(addr) = *self.service_status_rx.borrow_and_update() {
                return Poll::Ready(addr);
            }

            // Propagate the context to receive notification
            let fut = self.service_status_rx.changed();
            pin!(fut);

            match fut.poll(cx) {
                Poll::Ready(Ok(())) => {
                    // We can read the new value. Retry
                }
                _ => return Poll::Pending,
            }
        }
    }
}

pub fn run_scaler(options: ServiceScalerOptions) -> ServiceScaler {
    let (service_status_tx, service_status_rx) = watch::channel(ServiceStatus::Unhealthy);
    let request_notify: Arc<Notify> = Default::default();
    let result = ServiceScaler {
        service_status_rx,
        request_notify: request_notify.clone(),
    };

    spawn(async move {
        let mut next_health_check_time = Instant::now();
        let mut scale_down_time = Some(next_health_check_time + options.scale_down_period);
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
                _ = request_notify.notified() => {
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

                    scale_down_time = Some(Instant::now() + options.scale_down_period);
                }

                _ = sleep_until(target_time) => {
                    match timeout_action {
                        TimeoutAction::HealthCheck => {
                            // Check ECS status if nothing happened within 30 secs
                            debug!("Starting health check");

                            let result = match get_task_status(&options).await {
                                Ok(TaskStatus::Healthy) =>  match get_service_ip(&options).await {
                                    Ok(Some(addr)) => Ok(ServiceStatus::Healthy(addr)),
                                    Ok(None) => Ok(ServiceStatus::NoIP),
                                    Err(x) => Err(x)
                                }
                                Ok(_) => Ok(ServiceStatus::Unhealthy),
                                Err(x) => Err(x)
                            };

                            match result {
                                Ok(ref status) => send_new_status(&service_status_tx, status),
                                Err(x) => error!("Health check error: {}", x),
                            }

                            next_health_check_time = Instant::now() + HEALTH_CHECK_PERIOD;
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
        let service = services_res
            .services()
            .and_then(|services| match services {
                &[ref x] => Some(x),
                _ => None,
            })
            .context("The specified service is not found")?;
        service.desired_count()
    };

    if desired_count < 1 {
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
        match list_tasks_res.task_arns() {
            Some(x) if !x.is_empty() => x.iter().map(|x| x.clone()).collect(),
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
        .into_iter()
        .flat_map(|x| x.iter())
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
        match list_tasks_res.task_arns() {
            Some(x) if !x.is_empty() => x.iter().map(|x| x.clone()).collect(),
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
        .into_iter()
        .flat_map(|tasks| tasks.iter())
        .flat_map(|task| task.attachments().into_iter())
        .flat_map(|attachments| attachments.iter())
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
            .into_iter()
            .flat_map(|x| x.iter())
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
            .into_iter()
            .flat_map(|x| x.iter())
            .flat_map(|x| x.association().into_iter())
            .flat_map(|x| x.public_ip().into_iter())
            .nth(0)
            .context("No public IP is attached")?;
        return Ok(Some(IpAddr::from_str(public_ip)?));
    }

    let private_ip = attachment
        .details()
        .into_iter()
        .flat_map(|x| x.iter())
        .find_map(|x| match (x.name(), x.value()) {
            (Some("privateIPv4Address"), Some(private_ip)) => Some(private_ip),
            _ => None,
        })
        .context("Unable to get privateIPv4Address")?;
    Ok(Some(IpAddr::from_str(private_ip)?))
}

fn send_new_status(tx: &Sender<ServiceStatus>, status: &ServiceStatus) {
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
