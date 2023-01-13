use std::{mem, time::Duration};

use anyhow::{anyhow, Context, Result};
use futures::{AsyncRead, AsyncWrite, StreamExt};
use tokio::{net::UnixStream, sync::oneshot, task::JoinHandle};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
use tracing::Instrument;

use crate::meta::{ClientMeta, ServerMeta};

struct Services {}

#[derive(Debug, Clone)]
pub struct Config {
    pub socket_path: String,
    pub name: String,
}

impl Config {
    pub fn to_client_meta(&self) -> ClientMeta {
        ClientMeta {
            name: self.name.clone(),
        }
    }

    pub fn validate_server_meta(&self, server_meta: &ServerMeta) -> Result<()> {
        tracing::debug!(?server_meta, "validate server information");
        Ok(())
    }
}

pub struct Client {
    shutdown: oneshot::Sender<()>,
    join: JoinHandle<Result<()>>,
}

enum ClientState {
    Connect {
        config: Config,
    },
    Connected {
        config: Config,
        stream: UnixStream,
        server_meta: ServerMeta,
    },
    Invalid,
}

enum ClientSessionEvent {
    Shutdown,
    Accepted(yamux::Stream),
    Error(yamux::ConnectionError),
}

impl Client {
    pub async fn shutdown(self) {
        self.shutdown.send(()).ok();
        match self.join.await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::error!(?error, "session task failed"),
            Err(error) => tracing::error!(?error, "failed to join session task"),
        }
    }

    pub async fn connect(config: Config) -> Result<Self> {
        let (shutdown, shutdown_rx) = oneshot::channel();

        Ok(Self {
            shutdown,
            join: tokio::task::spawn(Self::run(config, shutdown_rx)),
        })
    }

    async fn discover_meta(stream: &mut UnixStream, config: &Config) -> Result<ServerMeta> {
        config
            .to_client_meta()
            .write_to_stream(&mut stream.compat())
            .await
            .context("failed to send client metadata")?;

        let server_meta = ServerMeta::read_from_stream(&mut stream.compat())
            .await
            .context("failed to read server metadata")?;

        config.validate_server_meta(&server_meta)?;
        Ok(server_meta)
    }

    async fn run(config: Config, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
        let mut state = ClientState::Connect { config };
        'client: loop {
            match mem::replace(&mut state, ClientState::Invalid) {
                ClientState::Connect { config } => {
                    match UnixStream::connect(config.socket_path.as_str()).await {
                        Ok(mut stream) => match tokio::select! {
                            _ = &mut shutdown => { Err(anyhow!("shut down")) }
                            res = Self::discover_meta(&mut stream, &config) => { res }
                        } {
                            Ok(server_meta) => {
                                tracing::info!(name = server_meta.name, "connected to server");
                                state = ClientState::Connected {
                                    config,
                                    stream,
                                    server_meta,
                                }
                            }
                            Err(error) => {
                                tracing::error!(?error, "failed to discover server metadata");
                                match tokio::time::timeout(Duration::from_secs(5), &mut shutdown)
                                    .await
                                {
                                    Err(_) => state = ClientState::Connect { config },
                                    Ok(_) => break Ok(()),
                                }
                            }
                        },
                        Err(error) => {
                            tracing::error!(?error, "connection failed");
                            match tokio::time::timeout(Duration::from_secs(5), &mut shutdown).await
                            {
                                Err(_) => state = ClientState::Connect { config },
                                Ok(_) => break Ok(()),
                            }
                        }
                    }
                }
                ClientState::Connected {
                    config,
                    stream,
                    server_meta,
                } => {
                    // create multiplexing controllers
                    let (ctl, connection) = yamux::Control::new(yamux::Connection::new(
                        stream.compat(),
                        yamux::Config::default(),
                        yamux::Mode::Client,
                    ));

                    // create second shutdown sender that will be handed to the session worker task
                    let (session_shutdown_tx, session_shutdown_rx) = oneshot::channel();

                    // start session worker task
                    let session_worker = tokio::task::spawn(
                        Self::run_connected(config, connection, session_shutdown_rx)
                            .instrument(tracing::error_span!("session-worker")),
                    );

                    // start client services
                    if let Err(e) = Self::start_services(ctl).await {
                        session_shutdown_tx.send(()).ok();
                        if let Err(error) = session_worker.await {
                            tracing::error!(?error, "failed to join session worker task");
                        }
                        break Err(e);
                    }

                    // wait for shutdown or the session worker task to exit
                    match tokio::select! {
                        // session worker exited
                        res = session_worker => {
                            match res {
                                Err(error) => {
                                    tracing::error!(?error, "failed to join session worker task");
                                    None
                                }
                                Ok(res) => res
                            }
                        }
                        // shutdown requested
                        _ = &mut shutdown => {
                            tracing::trace!("terminate session worker task");
                            session_shutdown_tx.send(()).ok();
                            None
                        }
                    } {
                        Some(new_state) => state = new_state,
                        None => break Ok(()),
                    }
                }
                ClientState::Invalid => unreachable!(),
            }
        }
    }

    async fn run_connected(
        config: Config,
        mut connection: yamux::ControlledConnection<Compat<UnixStream>>,
        mut shutdown: oneshot::Receiver<()>,
    ) -> Option<ClientState> {
        tracing::debug!("running session worker task");
        loop {
            match tokio::select! {
                r = connection.next() => {
                    match r {
                        Some(Ok(stream)) => ClientSessionEvent::Accepted(stream),
                        Some(Err(error)) => ClientSessionEvent::Error(error),
                        None => ClientSessionEvent::Error(yamux::ConnectionError::Closed)
                    }
                }
                _ = &mut shutdown => {
                    ClientSessionEvent::Shutdown
                }
            } {
                // drop any accepted stream
                ClientSessionEvent::Accepted(stream) => {
                    tracing::debug!("accepted new stream, dropping immediately");
                    drop(stream);
                }
                ClientSessionEvent::Shutdown => {
                    tracing::debug!("session shut down");
                    break None;
                }
                ClientSessionEvent::Error(error) => {
                    tracing::error!(?error, "session error");
                    break Some(ClientState::Connect {
                        config: config.clone(),
                    });
                }
            }
        }
    }

    async fn start_services(ctl: yamux::Control) -> Result<Services> {
        todo!()
    }
}
