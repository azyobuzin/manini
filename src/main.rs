use aws_sdk_ecs as ecs;
use clap::Parser;
use hyper::{Body, Request, Response, Server};
use std::net::SocketAddr;

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
    cluster: String,

    /// The service name
    #[clap(long, env = "MANINI_SERVICE", value_parser)]
    service: String,

    /// The port number on the container
    #[clap(long, env = "MANINI_TARGET_PORT", value_name = "PORT", value_parser)]
    target_port: u16,

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
    let ecs_client = ecs::Client::new(&aws_config::load_from_env().await);
    manini::run_scaler(ecs_client, cli.cluster, cli.service);

    //let server = Server::bind(&cli.bind).serve();
}

async fn shutdown_signal() {
    // https://hyper.rs/guides/server/graceful-shutdown/
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C signal handler");
}
