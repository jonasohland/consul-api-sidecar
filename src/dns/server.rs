use std::{collections::HashMap, io};

use anyhow::{Context, Result};
use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    AsyncRead, AsyncWrite, SinkExt, StreamExt,
};
use tracing::Instrument;

use crate::{
    dns::{egress_bridge, send_dns_message, DNSMessage, DNSMessageReader},
    task::{start_task, ShutdownHandle, TaskWrapper},
};

struct Session {
    task: TaskWrapper<Result<()>>,
}

enum Event {
    Shutdown,
    DNSFromBridge(DNSMessage),
    DNSFromClient(anyhow::Result<DNSMessage>),
}

impl Session {
    pub fn is_done(&self) -> bool {
        self.task.is_done()
    }

    pub async fn shutdown(mut self) {
        if let Ok(Err(error)) = self.task.shutdown().await {
            tracing::error!(?error, "failed to shutdown dns channel server");
        }
    }

    pub async fn run<S>(
        id: String,
        mut sock: S,
        mut shutdown: ShutdownHandle,
        mut tx: UnboundedSender<egress_bridge::BridgeMessage>,
        mut rx: UnboundedReceiver<DNSMessage>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin,
    {
        let mut msg_reader = DNSMessageReader::new();

        loop {
            match tokio::select! {
                _ = &mut shutdown => Event::Shutdown,
                res = rx.next() => match res {
                    Some(msg) => Event::DNSFromBridge(msg),
                    None => Event::Shutdown
                },
                res = msg_reader.read(&mut sock) => Event::DNSFromClient(res)
            } {
                Event::Shutdown => {
                    tracing::debug!("session shut down");
                    break Ok(());
                }
                Event::DNSFromBridge(msg) => {
                    if let Err(error) = send_dns_message(&mut sock, &msg).await {
                        break Err(error).context("send dns message to client");
                    }
                }
                Event::DNSFromClient(res) => match res {
                    Err(error) => {
                        if let Some(ioe) = error.downcast_ref::<io::Error>() {
                            if ioe.kind() == io::ErrorKind::UnexpectedEof {
                                tracing::debug!("channel closed");
                                break Ok(());
                            }
                        }
                        break Err(error).context("receive dns message from client");
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
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
    {
        let (responder, egress_bridge_rx) = unbounded();

        // register this new client
        egress_bridge_tx
            .send(egress_bridge::BridgeMessage::register(
                id.to_owned(),
                responder,
            ))
            .await?;

        Ok(Self {
            task: start_task(|shutdown| {
                Self::run(
                    id.to_owned(),
                    sock,
                    shutdown,
                    egress_bridge_tx,
                    egress_bridge_rx,
                )
                .instrument(tracing::error_span!("dns-channel-session", id))
            }),
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
        S: AsyncRead + AsyncWrite + Send + Sync + Unpin + 'static,
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
        let mut bridge = egress_bridge::Bridge::start(
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

        let mut reader = DNSMessageReader::new();

        assert_eq!(reader.read(&mut c2).await.unwrap().id(), 0xffee);
        assert_eq!(reader.read(&mut c2).await.unwrap().id(), 0xffff);
        assert_eq!(reader.read(&mut c2).await.unwrap().id(), 0xff33);

        server.shutdown().await;
        bridge.shutdown().await;
    }
}
