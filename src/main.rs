use aws_sdk_ec2 as ec2;
use aws_sdk_ecs as ecs;
use clap::Parser;
use hyper::Server;
use hyper::client::HttpConnector;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use log::{error, info};
use manini::{ProxyServiceOptions, ServiceScalerOptions, proxy_service_fn};
use std::convert::Infallible;
use std::future::ready;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

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

    /// Whether sends requests to the public IP address
    #[clap(long, env = "MANINI_PUBLIC_IP")]
    public_ip: bool,

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
    let service_scaler = {
        let config = aws_config::load_from_env().await;
        manini::run_scaler(ServiceScalerOptions {
            ecs_client: ecs::Client::new(&config),
            ec2_client: ec2::Client::new(&config),
            cluster_name: cli.cluster,
            service_name: cli.service,
            use_public_ip: cli.public_ip,
            scale_down_period: Duration::from_secs(cli.scale_down_period),
        })
    };

    let http_client = {
        let mut connector = HttpConnector::new();
        connector.set_keepalive(Some(Duration::from_secs(90))); // hyper::Client's default
        connector.set_connect_timeout(Some(Duration::from_secs(10)));
        hyper::Client::builder().build(connector)
    };

    let svc = make_service_fn(|conn: &AddrStream| {
        let service_options = ProxyServiceOptions {
            service_scaler: service_scaler.clone(),
            target_port: cli.target_port,
            http_client: http_client.clone(),
            remote_addr: conn.remote_addr().ip(),
        };

        ready(Ok::<_, Infallible>(service_fn(move |req| {
            let service_options = service_options.clone();
            async move { proxy_service_fn(req, &service_options).await }
        })))
    });
    let server = Server::bind(&cli.bind).serve(svc);

    info!("Listening on {}", cli.bind);

    match server.await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{}", e);
            ExitCode::FAILURE
        }
    }
}

#[test]
fn verify_cli() {
    use clap::CommandFactory;
    Cli::command().debug_assert()
}
