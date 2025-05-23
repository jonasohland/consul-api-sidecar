use std::io;

use anyhow::{Context, Result};
use futures::{
    channel::mpsc::{UnboundedReceiver, UnboundedSender},
    AsyncRead, AsyncWrite, SinkExt, StreamExt,
};
use tracing::Instrument;

use crate::{
    dns::{send_dns_message, DNSMessage, DNSMessageReader},
    task::{start_task, ShutdownHandle, TaskWrapper},
};

pub struct Client {
    task: TaskWrapper<Result<()>>,
}

enum Event {
    Shutdown,
    DNSFromBridge(DNSMessage),
    DNSFromServer(anyhow::Result<DNSMessage>),
}

impl Client {
    pub fn is_done(&self) -> bool {
        self.task.is_done()
    }

    pub async fn shutdown(&mut self) {
        if let Ok(Err(error)) = self.task.shutdown().await {
            tracing::error!(?error, "client task shutdown failed");
        }
    }

    pub fn start<S>(
        name: &str,
        sock: S,
        dns_tx: UnboundedSender<DNSMessage>,
        dns_rx: UnboundedReceiver<DNSMessage>,
    ) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        Self {
            task: start_task(|shutdown| {
                Self::run(sock, shutdown, dns_tx, dns_rx)
                    .instrument(tracing::error_span!("dns-channel-client", name))
            }),
        }
    }

    pub async fn run<S>(
        mut sock: S,
        mut shutdown: ShutdownHandle,
        mut dns_tx: UnboundedSender<DNSMessage>,
        mut dns_rx: UnboundedReceiver<DNSMessage>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    {
        let mut msg_reader = DNSMessageReader::new();
        loop {
            match tokio::select! {
                _ = &mut shutdown => Event::Shutdown,
                res = dns_rx.next() => match res {
                    Some(msg) => Event::DNSFromBridge(msg),
                    None => Event::Shutdown
                },
                res = msg_reader.read(&mut sock) => Event::DNSFromServer(res),
            } {
                Event::Shutdown => {
                    tracing::debug!("session shut down");
                    break Ok(());
                }
                Event::DNSFromBridge(msg) => {
                    if let Err(error) = send_dns_message(&mut sock, &msg).await {
                        break Err(error).context("send dns message to server");
                    }
                }
                Event::DNSFromServer(res) => match res {
                    Err(error) => {
                        if let Some(ioe) = error.downcast_ref::<io::Error>() {
                            if ioe.kind() == io::ErrorKind::UnexpectedEof {
                                tracing::debug!("channel closed");
                                break Ok(());
                            }
                        }
                        break Err(error).context("receive dns message from server");
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
