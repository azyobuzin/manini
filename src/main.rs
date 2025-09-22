use aws_sdk_ec2 as ec2;
use aws_sdk_ecs as ecs;
use clap::Parser;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use log::{error, info};
use manini::{
    ProxyHttpBody, ProxyServiceOptions, ServiceIp, ServiceScalerOptions, proxy_service_fn,
};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;
use tokio::net::TcpListener;

fn main() -> ExitCode {
    env_logger::init();
    let cli = Cli::parse();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async_main(cli))
}

#[derive(Parser)]
#[clap(author, version)]
struct Cli {
    /// The cluster name which contains the target service
    #[clap(long, env = "MANINI_CLUSTER", value_parser)]
    cluster: Option<String>,

    /// The service name
    #[clap(long, env = "MANINI_SERVICE", value_parser)]
    service: String,

    /// The port number on the container
    #[clap(long, env = "MANINI_TARGET_PORT", value_name = "PORT", value_parser)]
    target_port: u16,

    /// Which service IP address to target (private, public, or v6)
    #[clap(long, env = "MANINI_IP", value_enum, default_value_t = ServiceIp::Private)]
    ip: ServiceIp,

    /// The container will stop if no requests are received for the specified time
    #[clap(
        long,
        env = "MANINI_SCALE_DOWN_PERIOD",
        value_name = "SECS",
        value_parser,
        default_value_t = 300u64
    )]
    scale_down_period: u64,

    /// The address to be binded the manini proxy
    #[clap(
        long,
        env = "MANINI_BIND",
        value_name = "ADDR",
        value_parser,
        default_value = "0.0.0.0:3000"
    )]
    bind: SocketAddr,
}

async fn async_main(cli: Cli) -> ExitCode {
    let Cli {
        cluster,
        service,
        target_port,
        ip,
        scale_down_period,
        bind,
    } = cli;

    let service_scaler = {
        let config = aws_config::load_from_env().await;
        manini::run_scaler(ServiceScalerOptions {
            ecs_client: ecs::Client::new(&config),
            ec2_client: ec2::Client::new(&config),
            cluster_name: cluster,
            service_name: service,
            ip_selection: ip,
            scale_down_period: Duration::from_secs(scale_down_period),
        })
    };

    let http_client: Client<_, ProxyHttpBody> = {
        let mut connector = HttpConnector::new();
        connector.set_keepalive(Some(Duration::from_secs(90)));
        connector.set_connect_timeout(Some(Duration::from_secs(10)));
        Client::builder(TokioExecutor::new()).build(connector)
    };

    let listener = match TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(e) => {
            error!("Failed to bind to {}: {}", bind, e);
            return ExitCode::FAILURE;
        }
    };

    info!("Listening on {}", bind);

    let connection_builder = auto::Builder::new(TokioExecutor::new());

    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("Accept error: {}", e);
                continue;
            }
        };

        let service_options = ProxyServiceOptions {
            service_scaler: service_scaler.clone(),
            target_port,
            http_client: http_client.clone(),
            remote_addr: addr.ip(),
        };

        let builder = connection_builder.clone();

        tokio::spawn(async move {
            let service = service_fn(move |req| {
                let service_options = service_options.clone();
                async move { proxy_service_fn(req, &service_options).await }
            });

            let io = TokioIo::new(stream);

            if let Err(err) = builder.serve_connection_with_upgrades(io, service).await {
                error!("Error serving connection from {}: {}", addr, err);
            }
        });
    }
}

#[test]
fn verify_cli() {
    use clap::CommandFactory;
    Cli::command().debug_assert()
}
