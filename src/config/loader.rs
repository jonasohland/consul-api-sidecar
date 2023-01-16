use crate::task::{start_task, ShutdownHandle, TaskWrapper};

use anyhow::{Context, Result};
use futures::{channel::mpsc::UnboundedSender, SinkExt};
use tokio::signal::{self, unix::SignalKind};

pub struct ConfigLoader {
    task: TaskWrapper<Result<()>>,
}

impl ConfigLoader {
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Ok(res) = self.task.shutdown().await {
            res?
        }
        Ok(())
    }

    pub async fn wait(&mut self) -> Result<Option<Result<()>>> {
        self.task.wait().await
    }
}

enum Event {
    Shutdown,
    Reload,
}

pub fn start<C>(path: String, tx: UnboundedSender<C>) -> ConfigLoader
where
    C: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    ConfigLoader {
        task: start_task(|shutdown| run(shutdown, path, tx)),
    }
}

pub async fn reload<C>(path: &str, tx: &mut UnboundedSender<C>) -> Result<()>
where
    C: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    tracing::info!(path, "config reload triggered");
    let config: C = toml::from_slice(
        &tokio::fs::read(path)
            .await
            .context("failed to read config file")?,
    )
    .context("failed to parse config file")?;
    tx.send(config).await.ok();
    Ok(())
}

pub async fn run<C>(
    mut shutdown: ShutdownHandle,
    path: String,
    mut tx: UnboundedSender<C>,
) -> Result<()>
where
    C: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let mut signal =
        signal::unix::signal(SignalKind::hangup()).context("failed to install signal handler")?;

    reload(path.as_str(), &mut tx)
        .await
        .context("initial reload failed")?;

    loop {
        match tokio::select! {
            _ = &mut shutdown => Event::Shutdown,
            _ = signal.recv() => Event::Reload
        } {
            Event::Shutdown => break Ok(()),
            Event::Reload => {
                if let Err(error) = reload(&path, &mut tx).await {
                    tracing::error!(?error, "config file reload failed")
                }
            }
        }
    }
}
