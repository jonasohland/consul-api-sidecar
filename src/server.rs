use std::time::Duration;

use anyhow::anyhow;
use tokio::net::UnixListener;

use tokio::net::unix::SocketAddr;
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use anyhow::Result;
use tokio_util::compat::TokioAsyncReadCompatExt;

use crate::handshake::frame::Frame;

struct Server {
    join_handle: JoinHandle<Result<()>>,
    shutdown_handle: oneshot::Sender<()>,
}

enum ServerEvent {
    Accepted(Result<(UnixStream, SocketAddr)>),
    Shutdown,
}

impl Server {
    async fn shutdown(self) -> Result<()> {
        match self.shutdown_handle.send(()) {
            Ok(_) => self.join_handle.await?,
            Err(_) => Err(anyhow!("already shut down")),
        }
    }

    async fn run_server(listener: UnixListener, mut shutdown: oneshot::Receiver<()>) -> Result<()> {
        loop {
            match tokio::select! {
                res = listener.accept() => {
                    ServerEvent::Accepted(res.map_err(Into::into))
                }
                _ = &mut shutdown => {
                    ServerEvent::Shutdown
                }
            } {
                ServerEvent::Accepted(Ok((nix_stream, addr))) => {}
                ServerEvent::Accepted(Err(err)) => break Err(err),
                ServerEvent::Shutdown => {
                    println!("shutting down");
                    break Ok(());
                }
            }
        }
    }

    pub async fn bind(path: &str) -> Result<Self> {
        let (shutdown_handle, shutdown) = oneshot::channel();
        let listener = UnixListener::bind(path)?;
        Ok(Self {
            join_handle: tokio::task::spawn(Self::run_server(listener, shutdown)),
            shutdown_handle,
        })
    }
}
