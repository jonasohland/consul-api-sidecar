use std::{
    collections::HashMap,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use futures::{
    channel::{
        mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    AsyncRead, AsyncWrite, SinkExt, StreamExt,
};
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::dns::{egress_bridge, receive_dns_message, send_dns_message, DNSMessage};

struct Session {
    task: JoinHandle<Result<()>>,
    shutdown: oneshot::Sender<()>,
    done: Arc<AtomicBool>,
}

enum Event {
    Shutdown,
    DNSFromBridge(DNSMessage),
    DNSFromChannel(anyhow::Result<DNSMessage>),
}

impl Session {
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    pub async fn shutdown(self) {
        self.shutdown.send(()).ok();
        match self.task.await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::error!(?error, "session task failed"),
            Err(error) => tracing::error!(?error, "failed to join session task"),
        }
    }

    pub async fn run<S>(
        id: String,
        mut sock: S,
        mut shutdown: oneshot::Receiver<()>,
        done: Arc<AtomicBool>,
        mut tx: UnboundedSender<egress_bridge::BridgeMessage>,
        mut rx: UnboundedReceiver<DNSMessage>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin,
    {
        scopeguard::guard(done, |done| done.store(true, Ordering::Relaxed));
        loop {
            match tokio::select! {
                _ = &mut shutdown => {
                    Event::Shutdown
                }
                res = rx.next() => {
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
                    break Ok(())
                },
                Event::DNSFromBridge(msg) => {
                    if let Err(error) = send_dns_message(&mut sock, &msg).await {
                        tracing::warn!(?error, "failed to send dns message to channel")
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
                        if let Err(error) = tx
                            .send(egress_bridge::BridgeMessage::dns(id.clone(), msg))
                            .await
                        {
                            tracing::warn!(?error, "failed to send dns message to bridge");
                        }
                    }
                },
            }
        }
    }

    pub async fn launch<S>(
        id: &str,
        sock: S,
        mut egress_bridge_tx: UnboundedSender<egress_bridge::BridgeMessage>,
    ) -> Result<Self>
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let done = Arc::new(AtomicBool::new(false));
        let (shutdown, shutdown_rx) = oneshot::channel();
        let (responder, egress_bridge_rx) = unbounded();

        // register this new client
        egress_bridge_tx
            .send(egress_bridge::BridgeMessage::register(
                id.to_owned(),
                responder,
            ))
            .await?;

        Ok(Self {
            task: tokio::task::spawn(
                Self::run(
                    id.to_owned(),
                    sock,
                    shutdown_rx,
                    done.clone(),
                    egress_bridge_tx,
                    egress_bridge_rx,
                )
                .instrument(tracing::error_span!("dns-channel-session", id)),
            ),
            shutdown,
            done,
        })
    }
}

pub struct Server {
    sessions: HashMap<String, Session>,
    egress_bridge_tx: UnboundedSender<egress_bridge::BridgeMessage>,
}

impl Server {
    pub fn new(egress_bridge_tx: UnboundedSender<egress_bridge::BridgeMessage>) -> Self {
        Self {
            sessions: Default::default(),
            egress_bridge_tx,
        }
    }

    pub fn cleanup(&mut self) {
        self.sessions = self
            .sessions
            .drain()
            .filter(|(_, session)| !session.is_done())
            .collect()
    }

    pub async fn shutdown(&mut self) {
        for (_, session) in self.sessions.drain() {
            session.shutdown().await
        }
    }

    pub async fn accept<S>(&mut self, id: &str, sock: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        tracing::debug!(id, "accept new session");
        match Session::launch(id, sock, self.egress_bridge_tx.clone()).await {
            Ok(session) => {
                if let Some(old) = self.sessions.insert(id.to_owned(), session) {
                    old.shutdown().await;
                }
            }
            Err(error) => {
                tracing::error!(?error, "failed to accept new dns channel session")
            }
        }
        self.cleanup();
    }
}

#[allow(unused)]
mod test {

    use bytes::BytesMut;

    use crate::{dns::forwarder::Forwarder, testing::make_test_connection};

    use super::*;

    #[tokio::test]
    async fn e2e() {
        let (br_tx, br_rx) = unbounded();
        let mut server = Server::new(br_tx);
        let bridge = egress_bridge::Bridge::start(
            egress_bridge::Config::default(),
            Forwarder::loopback(),
            br_rx,
        )
        .unwrap();

        let (c1, mut c2) = make_test_connection();
        server.accept("test_1", c1).await;

        send_dns_message(
            &mut c2,
            &DNSMessage::try_new(BytesMut::from([0xff, 0xee].as_slice())).unwrap(),
        )
        .await;
        send_dns_message(
            &mut c2,
            &DNSMessage::try_new(BytesMut::from([0xff, 0xff].as_slice())).unwrap(),
        )
        .await;
        send_dns_message(
            &mut c2,
            &DNSMessage::try_new(BytesMut::from([0xff, 0x33].as_slice())).unwrap(),
        )
        .await;

        assert_eq!(receive_dns_message(&mut c2).await.unwrap().id(), 0xffee);
        assert_eq!(receive_dns_message(&mut c2).await.unwrap().id(), 0xffff);
        assert_eq!(receive_dns_message(&mut c2).await.unwrap().id(), 0xff33);

        server.shutdown().await;
        bridge.shutdown().await.unwrap();
    }
}
