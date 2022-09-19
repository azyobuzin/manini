use aws_sdk_ec2 as ec2;
use aws_sdk_ecs as ecs;
use clap::Parser;
use hyper::{Body, Request, Response, Server};
use manini::ServiceScalerOptions;
use std::net::SocketAddr;
use std::time::Duration;

fn main() {
    env_logger::init();
    let cli = Cli::parse();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { async_main(cli).await })
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

async fn async_main(cli: Cli) {
    let config = aws_config::load_from_env().await;
    manini::run_scaler(ServiceScalerOptions {
        ecs_client: ecs::Client::new(&config),
        ec2_client: ec2::Client::new(&config),
        cluster_name: cli.cluster,
        service_name: cli.service,
        use_public_ip: cli.public_ip,
        scale_down_period: Duration::from_secs(cli.scale_down_period),
    });

    //let server = Server::bind(&cli.bind).serve();
}

async fn shutdown_signal() {
    // https://hyper.rs/guides/server/graceful-shutdown/
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}
