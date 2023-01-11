use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use futures::{
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    AsyncRead, AsyncWrite, SinkExt, StreamExt,
};
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::dns::{receive_dns_message, send_dns_message, DNSMessage};

pub struct Client {
    done: Arc<AtomicBool>,
    shutdown: oneshot::Sender<()>,
    join: JoinHandle<Result<()>>,
}

enum Event {
    Shutdown,
    DNSFromBridge(DNSMessage),
    DNSFromChannel(anyhow::Result<DNSMessage>),
}

impl Client {

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

    pub fn start<S>(
        id: &str,
        sock: S,
        dns_tx: UnboundedSender<DNSMessage>,
        dns_rx: UnboundedReceiver<DNSMessage>,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        let done = Arc::new(AtomicBool::new(false));
        let (shutdown, shutdown_rx) = oneshot::channel();
        Self {
            done: done.clone(),
            shutdown,
            join: tokio::task::spawn(
                Self::run(sock, shutdown_rx, done, dns_tx, dns_rx)
                    .instrument(tracing::error_span!("dns-channel-client", id)),
            ),
        }
    }

    pub async fn run<S>(
        mut sock: S,
        mut shutdown: oneshot::Receiver<()>,
        done: Arc<AtomicBool>,
        mut dns_tx: UnboundedSender<DNSMessage>,
        mut dns_rx: UnboundedReceiver<DNSMessage>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        scopeguard::guard(done, |done| done.store(true, Ordering::Relaxed));
        loop {
            match tokio::select! {
                _ = &mut shutdown => {
                    Event::Shutdown
                }
                res = dns_rx.next() => {
                    match res {
                        Some(msg) => Event::DNSFromBridge(msg),
                        None => Event::Shutdown
                    }
                }
                res = receive_dns_message(&mut sock) => {
                    Event::DNSFromChannel(res)
                }
            } {
                Event::Shutdown => {
                    tracing::debug!("session shut down");
                    break Ok(());
                }
                Event::DNSFromBridge(msg) => {
                    if let Err(error) = send_dns_message(&mut sock, &msg).await {
                        tracing::debug!(?error, "failed to send dns message to channel")
                    }
                }
                Event::DNSFromChannel(res) => match res {
                    Err(error) => {
                        if let Some(ioe) = error.downcast_ref::<io::Error>() {
                            if ioe.kind() == io::ErrorKind::UnexpectedEof {
                                tracing::debug!("channel closed");
                                break Ok(());
                            }
                        }
                        tracing::warn!(?error, "failed to receive dns message from channel");
                        break Err(error).context("failed to receive dns message from channel");
                    }
                    Ok(msg) => {
                        if let Err(error) = dns_tx.send(msg).await {
                            tracing::warn!(?error, "failed to send dns message to bridge")
                        }
                    }
                },
            }
        }
    }
}


