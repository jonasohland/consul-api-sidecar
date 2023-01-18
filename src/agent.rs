use std::process;

use clap::Parser;
use consul_api_sidecar::{
    config,
    dns,
    service::{self, ServiceLauncher},
    task::TaskWrapper,
    tcp,
};
use futures::channel::mpsc::unbounded;

#[derive(clap::Parser)]
struct Cli {
    #[arg(long, short, rename_all = "lowercase", default_value = "info")]
    log_level: tracing::Level,

    #[arg(long, short)]
    config: String,
}

#[tokio::main(flavor = "current_thread")]
pub async fn main() {
    _main().await
}

struct Launcher;

#[async_trait::async_trait]
impl ServiceLauncher<config::agent::Config> for Launcher {
    type Task = TaskWrapper<()>;

    #[rustfmt::skip]
    async fn launch(&mut self, name: &str, config: config::agent::ServiceConfig) -> Self::Task {
        match config {
            config::agent::ServiceConfig::DNS { path, address, timeout } => dns::service::start_server(name, path, address, timeout),
            config::agent::ServiceConfig::TCP { path, address } => tcp::service::start_server(name, path, address),
        }
    }

    async fn stop(&mut self, _name: &str, mut task: Self::Task) {
        task.shutdown().await.ok();
    }
}

async fn _main() {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(cli.log_level)
        .init();

    let (cfg_tx, cfg_rx) = unbounded();

    let mut config_loader = config::loader::start(cli.config.clone(), cfg_tx);
    let mut services = service::start(cfg_rx, Launcher);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        res = config_loader.wait() => {
            if let Ok(Some(Err(error))) = res {
                tracing::error!(?error, "config loader failed");
                process::exit(1);
            }
        }
    }

    tracing::debug!("shutting down");
    config_loader.shutdown().await.ok();
    services.shutdown().await;
}
