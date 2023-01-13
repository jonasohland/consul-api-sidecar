#![allow(unused)]

use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::anyhow;
use futures::{AsyncWriteExt, StreamExt};
use tokio::net::UnixListener;

use tokio::net::unix::SocketAddr;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use anyhow::Result;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::Instrument;
use yamux::ConnectionError;

use crate::{
    client, handshake,
    meta::{ClientMeta, ServerMeta},
};

#[derive(Clone)]
pub struct Config {
    pub name: String,
    pub socket_path: String,
}

impl Config {
    fn to_server_meta(&self) -> ServerMeta {
        ServerMeta {
            name: self.name.clone(),
        }
    }
}

struct Session {
    done: Arc<AtomicBool>,
    shutdown: oneshot::Sender<()>,
    join: JoinHandle<Result<()>>,
}

enum SessionEvent {
    Shutdown,
    Accepted(Result<yamux::Stream, yamux::ConnectionError>),
}

impl Session {
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub async fn shutdown(self) {
        self.shutdown.send(()).ok();
        match self.join.await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::error!(?error, "session task failed"),
            Err(error) => tracing::error!(?error, "failed to join session task"),
        }
    }

    fn start(stream: UnixStream, config: Config) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let (shutdown, shutdown_rx) = oneshot::channel();
        Self {
            done: done.clone(),
            shutdown,
            join: tokio::task::spawn(Self::run(shutdown_rx, done, stream, config)),
        }
    }

    async fn initial_handshake(s: &mut UnixStream, config: &Config) -> Result<ClientMeta> {
        let client_meta = ClientMeta::read_from_stream(&mut s.compat()).await?;
        tracing::debug!(
            name = client_meta.name,
            "received client information, send server info"
        );
        config
            .to_server_meta()
            .write_to_stream(&mut s.compat())
            .await?;
        tracing::debug!(name = client_meta.name, "handshake complete, start session");
        Ok(client_meta)
    }

    async fn run(
        mut shutdown: oneshot::Receiver<()>,
        done: Arc<AtomicBool>,
        mut stream: UnixStream,
        config: Config,
    ) -> Result<()> {
        scopeguard::guard(done, |done| done.store(true, Ordering::Relaxed));

        // perform initial handshake
        let client_meta = tokio::select! {
            _ = &mut shutdown => { Err(anyhow!("shut down during initial handshake")) }
            res = Self::initial_handshake(&mut stream, &config) => res
        }?;

        // create multiplexing controller
        let (_ctl, mut con) = yamux::Control::new(yamux::Connection::new(
            stream.compat(),
            yamux::Config::default(),
            yamux::Mode::Server,
        ));

        // start working loop
        async move {
            loop {
                match tokio::select! {
                    _ = &mut shutdown => SessionEvent::Shutdown,
                    res = con.next() => {
                        match res {
                            Some(r) => SessionEvent::Accepted(r),
                            None => SessionEvent::Shutdown
                        }
                    }
                } {
                    SessionEvent::Shutdown => {
                        tracing::debug!("session shut down");
                        break Ok(());
                    }
                    SessionEvent::Accepted(Err(error)) => match error {
                        ConnectionError::Closed => {
                            tracing::debug!("session shut down");
                            break Ok(());
                        }
                        error => {
                            tracing::error!(?error, "session error");
                            break Err(error.into());
                        }
                    },
                    SessionEvent::Accepted(Ok(stream)) => {}
                }
            }
        }
        .instrument(tracing::error_span!(
            "client-session",
            name = client_meta.name
        ))
        .await
    }
}

pub struct Server {
    join_handle: JoinHandle<Result<()>>,
    shutdown_handle: oneshot::Sender<()>,
}

enum ServerEvent {
    Accepted(Result<(UnixStream, SocketAddr)>),
    Shutdown,
}

impl Server {
    pub async fn shutdown(self) -> Result<()> {
        match self.shutdown_handle.send(()) {
            Ok(_) => self.join_handle.await?,
            Err(_) => Err(anyhow!("already shut down")),
        }
    }

    async fn on_accept(stream: UnixStream, config: &Config) -> Result<Session> {
        Ok(Session::start(stream, config.clone()))
    }

    async fn cleanup(sessions: &mut Vec<Session>) {
        let mut new = Vec::new();
        let mut shutdown = Vec::new();
        for session in sessions.drain(0..sessions.len()) {
            if session.is_done() {
                shutdown.push(session);
            } else {
                new.push(session)
            }
        }
        *sessions = new;
        if !shutdown.is_empty() {
            tracing::debug!(count = shutdown.len(), "clean up old sessions");
            futures::future::select_all(shutdown.into_iter().map(|s| Box::pin(s.shutdown()))).await;
        }
    }

    async fn run_server(
        listener: UnixListener,
        mut shutdown: oneshot::Receiver<()>,
        config: Config,
    ) -> Result<()> {
        let mut sessions = Vec::new();
        let res = loop {
            match tokio::select! {
                res = listener.accept() => ServerEvent::Accepted(res.map_err(Into::into)),
                _ = &mut shutdown => ServerEvent::Shutdown
            } {
                ServerEvent::Accepted(Ok((stream, addr))) => {
                    match tokio::select! {
                        res = Self::on_accept(stream, &config) => res,
                        _  = &mut shutdown => Err(anyhow::anyhow!("shut down"))
                    } {
                        Ok(session) => {
                            sessions.push(session);
                            tracing::debug!(?addr, "connection accepted")
                        }
                        Err(error) => {
                            tracing::error!(?addr, ?error, "failed to accept connection")
                        }
                    }
                }
                ServerEvent::Accepted(Err(err)) => break Err(err),
                ServerEvent::Shutdown => {
                    tracing::debug!("shut down");
                    break Ok(());
                }
            }
            if tokio::select! {
                _ = Self::cleanup(&mut sessions) => false,
                _ = &mut shutdown => true
            } {
                break Ok(());
            }
        };
        for session in sessions {
            session.shutdown().await
        }
        res
    }

    pub async fn bind(config: Config) -> Result<Self> {
        let (shutdown_handle, shutdown) = oneshot::channel();
        let listener = UnixListener::bind(config.socket_path.as_str())?;
        Ok(Self {
            join_handle: tokio::task::spawn(Self::run_server(listener, shutdown, config)),
            shutdown_handle,
        })
    }
}
