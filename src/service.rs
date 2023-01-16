use std::collections::{hash_map, HashMap};

use futures::{channel::mpsc::UnboundedReceiver, StreamExt};

use crate::{
    config,
    task::{start_task, ShutdownHandle, TaskWrapper},
};

pub struct ServiceRunner {
    task: TaskWrapper<()>,
}

impl ServiceRunner {
    pub async fn shutdown(&mut self) {
        self.task.shutdown().await.ok();
    }
}

#[async_trait::async_trait]
pub trait ServiceLauncher<C>
where
    C: config::Config,
{
    type Task: Send + 'static;
    async fn launch(&mut self, name: &str, config: C::ServiceConfig) -> Self::Task;
    async fn stop(&mut self, name: &str, task: Self::Task);
}

enum Event<C> {
    Shutdown,
    Reload(C),
}

pub fn start<C, L>(rx: UnboundedReceiver<C>, launcher: L) -> ServiceRunner
where
    L: ServiceLauncher<C> + Send + 'static,
    C: config::Config + Send + 'static,
    <C as config::Config>::ServiceConfig: Clone + Sync + Send + 'static,
{
    ServiceRunner {
        task: start_task(|shutdown| run(shutdown, rx, launcher)),
    }
}
pub async fn run<C, L>(mut shutdown: ShutdownHandle, mut rx: UnboundedReceiver<C>, mut launcher: L)
where
    L: ServiceLauncher<C>,
    C: config::Config + Send + 'static,
    <C as config::Config>::ServiceConfig: Clone + Send + 'static,
{
    let mut services = HashMap::<String, (C::ServiceConfig, L::Task)>::new();
    loop {
        match tokio::select! {
            _ = &mut shutdown => {
                Event::Shutdown
            }
            res = rx.next() => {
                match res {
                    None => Event::Shutdown,
                    Some(config) => Event::Reload(config)
                }
            }
        } {
            Event::Shutdown => {
                for (name, (_, task)) in services.drain() {
                    tracing::debug!(name, "shut down service task");
                    launcher.stop(name.as_str(), task).await;
                }
                break;
            }
            Event::Reload(config) => {
                // get a list of updated services
                let updated_services = config.into_services();

                // start new services, or update existing ones
                for (name, service_config) in &updated_services {
                    match services.entry(name.clone()) {
                        hash_map::Entry::Occupied(entry) => {
                            if entry.get().0 != *service_config {
                                tracing::info!(name, "service configuration changed");
                                launcher.stop(name, entry.remove().1).await;
                                services.insert(
                                    name.clone(),
                                    (
                                        service_config.clone(),
                                        launcher.launch(name, service_config.clone()).await,
                                    ),
                                );
                            }
                        }
                        hash_map::Entry::Vacant(entry) => {
                            tracing::info!(name, "new service");
                            entry.insert((
                                service_config.clone(),
                                launcher.launch(name, service_config.clone()).await,
                            ));
                        }
                    }
                }

                // because of borrowing rules we allocate a new list of removed services
                let to_remove = services
                    .iter()
                    .filter(|(name, _)| !updated_services.contains_key(name.as_str()))
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();

                for name in to_remove {
                    tracing::info!(name, "remove service");
                    if let Some((_, task)) = services.remove(&name) {
                        launcher.stop(name.as_str(), task).await;
                    }
                }
            }
        }
    }
}
