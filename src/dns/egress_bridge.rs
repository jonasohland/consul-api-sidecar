use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use futures::{
    channel::{
        mpsc::{UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    SinkExt, StreamExt,
};
use tokio::{task::JoinHandle, time::Instant};
use tracing::Instrument;

use super::{forwarder::Forwarder, DNSMessage};

pub struct Bridge {
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
}

pub struct Config {
    message_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_timeout: Duration::from_secs(100),
        }
    }
}

pub enum BridgeMessage {
    Register(String, UnboundedSender<DNSMessage>),
    DnsMessage(String, DNSMessage),
}

impl BridgeMessage {
    pub fn register(id: String, responder: UnboundedSender<DNSMessage>) -> Self {
        Self::Register(id, responder)
    }

    pub fn dns(id: String, msg: DNSMessage) -> Self {
        Self::DnsMessage(id, msg)
    }
}

enum BridgeEvent {
    Shutdown,
    Timeout,
    Registration(String, UnboundedSender<DNSMessage>),
    DNSFromNet(DNSMessage),
    DNSFromBridge(String, DNSMessage),
}

struct MessageState {
    creation_time: Instant,
    client: String,
}

impl MessageState {
    pub fn new(client: String) -> Self {
        Self {
            creation_time: Instant::now(),
            client,
        }
    }
}

impl Bridge {
    pub async fn shutdown(self) -> Result<()> {
        self.shutdown.send(()).ok();
        self.task.await??;
        Ok(())
    }

    pub fn start(
        config: Config,
        forwarder: Forwarder,
        rx: UnboundedReceiver<BridgeMessage>,
    ) -> Result<Self> {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        Ok(Self {
            shutdown: shutdown_tx,
            task: tokio::task::spawn(
                Self::run(config, shutdown_rx, forwarder, rx)
                    .instrument(tracing::error_span!("dns-egress-bridge")),
            ),
        })
    }

    fn cleanup_messages(messages: &mut HashMap<u16, MessageState>, config: &Config) {
        let now = Instant::now();
        *messages = messages
            .drain()
            .filter(|(id, state)| {
                if now
                    .checked_duration_since(state.creation_time)
                    .map(|d| d < config.message_timeout)
                    .unwrap_or(false)
                {
                    // keep
                    true
                } else {
                    tracing::warn!(id, client_id = ?state.client, "remove expired request");
                    false
                }
            })
            .collect::<HashMap<_, _>>();
    }

    fn cleanup_clients(clients: &mut HashMap<String, UnboundedSender<DNSMessage>>) {
        *clients = clients
            .drain()
            .filter(|(id, sender)| {
                if sender.is_closed() {
                    tracing::debug!(id, "cleaning up dead client connection");
                    false
                } else {
                    true
                }
            })
            .collect();
    }

    async fn run(
        config: Config,
        mut shutdown: oneshot::Receiver<()>,
        mut forwarder: Forwarder,
        mut rx: UnboundedReceiver<BridgeMessage>,
    ) -> Result<()> {
        let mut clients = HashMap::<String, UnboundedSender<DNSMessage>>::new();
        let mut messages = HashMap::<u16, MessageState>::new();
        loop {
            match tokio::select! {
                _ = &mut shutdown => {
                    BridgeEvent::Shutdown
                }
                _ = tokio::time::sleep(Duration::from_secs(3)) => {
                    BridgeEvent::Timeout
                }
                res = forwarder.next() => {
                    match res {
                        Some(msg) => BridgeEvent::DNSFromNet(msg),
                        None => BridgeEvent::Shutdown,
                    }

                }
                res = rx.next() => {
                    match res {
                        Some(BridgeMessage::Register(id, responder)) => BridgeEvent::Registration(id, responder),
                        Some(BridgeMessage::DnsMessage(id, msg)) => BridgeEvent::DNSFromBridge(id, msg),
                        None => BridgeEvent::Shutdown
                    }
                }
            } {
                BridgeEvent::Shutdown => {
                    tracing::debug!("shutting down");
                    break Ok(());
                }
                BridgeEvent::Timeout => {
                    // run loop to clean up
                }
                BridgeEvent::Registration(id, tx) => {
                    tracing::debug!(id, "new client registered");
                    if clients.insert(id.clone(), tx).is_some() {
                        tracing::warn!(id, "dropping previously registered client channel")
                    }
                }
                BridgeEvent::DNSFromNet(msg) => match messages.remove(&msg.id()) {
                    Some(state) => {
                        let id = msg.id();
                        match clients.get_mut(&state.client) {
                            Some(client) => {
                                if let Err(error) = client.send(msg).await {
                                    tracing::warn!(
                                        id,
                                        client_id = state.client,
                                        ?error,
                                        "failed to forward dns response to client"
                                    )
                                }
                            }
                            None => {
                                tracing::warn!(
                                    id,
                                    client_id = state.client,
                                    "no client registered for dns message"
                                )
                            }
                        }
                    }
                    None => {
                        tracing::trace!(id = msg.id(), "dropping orphaned dns message")
                    }
                },
                BridgeEvent::DNSFromBridge(client_id, msg) => {
                    let id = msg.id();
                    if let Err(error) = forwarder.send(msg).await {
                        tracing::warn!(
                            id,
                            client_id,
                            ?error,
                            "failed to send dns message to forwarder"
                        )
                    } else if let Some(prev) =
                        messages.insert(id, MessageState::new(client_id.clone()))
                    {
                        tracing::warn!(id, client_id, "msg id collision");
                        messages.insert(id, prev);
                    }
                }
            }
            Self::cleanup_clients(&mut clients);
            Self::cleanup_messages(&mut messages, &config)
        }
    }
}

#[allow(unused)]
mod test {
    use bytes::BytesMut;
    use futures::channel::mpsc::unbounded;
    use rand::{distributions::Alphanumeric, Rng};

    use crate::testing::ManualLoopback;

    use super::*;

    pub struct MockClient {
        from_bridge: UnboundedReceiver<DNSMessage>,
        to_bridge: UnboundedSender<BridgeMessage>,
        id: String,
    }

    impl MockClient {
        pub async fn with_id(id: &str, mut to_bridge: UnboundedSender<BridgeMessage>) -> Self {
            let (responder, from_bridge) = unbounded();
            to_bridge
                .send(BridgeMessage::register(id.to_owned(), responder))
                .await
                .unwrap();
            Self {
                from_bridge,
                to_bridge,
                id: id.to_owned(),
            }
        }

        pub async fn new(to_bridge: UnboundedSender<BridgeMessage>) -> Self {
            let id: String = rand::thread_rng()
                .sample_iter(&Alphanumeric)
                .take(16)
                .map(char::from)
                .collect();
            Self::with_id(&id, to_bridge).await
        }

        async fn send_dns_msg(&mut self, id: u16) {
            self.to_bridge
                .send(BridgeMessage::DnsMessage(
                    self.id.clone(),
                    DNSMessage::try_new(BytesMut::from(id.to_be_bytes().as_slice())).unwrap(),
                ))
                .await
                .unwrap();
        }

        async fn receive_dns_msg(&mut self) -> DNSMessage {
            self.from_bridge.next().await.unwrap()
        }
    }

    #[tokio::test]
    async fn shutdown() {
        let (_tx, rx) = unbounded();
        let bridge = Bridge::start(Config::default(), Forwarder::loopback(), rx).unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        bridge.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loopback() {
        let (mut tx_dns, rx_dns) = unbounded();

        let bridge = Bridge::start(Config::default(), Forwarder::loopback(), rx_dns).unwrap();

        let mut client = MockClient::new(tx_dns).await;

        client.send_dns_msg(0xffff).await;
        assert_eq!(client.receive_dns_msg().await.id(), 0xffff);
        bridge.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn collision() {
        let (mut tx_dns, rx_dns) = unbounded();
        let (to_bridge, fwd_rx) = unbounded();
        let (fwd_tx, from_bridge) = unbounded();
        let mut loopback = ManualLoopback::new(to_bridge, from_bridge);

        let bridge =
            Bridge::start(Config::default(), Forwarder::manual(fwd_tx, fwd_rx), rx_dns).unwrap();

        let mut client = MockClient::new(tx_dns).await;

        client.send_dns_msg(0xffff).await;
        client.send_dns_msg(0xffff).await;

        loopback.loopback().await.unwrap();

        assert_eq!(client.receive_dns_msg().await.id(), 0xffff);

        bridge.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn client_cleanup() {
        let (mut tx_dns, rx_dns) = unbounded();
        let bridge = Bridge::start(Config::default(), Forwarder::loopback(), rx_dns).unwrap();
        {
            let mut client = MockClient::with_id("static", tx_dns.clone()).await;
            client.send_dns_msg(0xffff).await;
            assert_eq!(client.receive_dns_msg().await.id(), 0xffff);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        {
            let mut client = MockClient::with_id("static", tx_dns.clone()).await;
            client.send_dns_msg(0xffff).await;
            assert_eq!(client.receive_dns_msg().await.id(), 0xffff);
        }
        bridge.shutdown().await.unwrap()
    }

    #[tokio::test]
    async fn msg_timeout() {
        let (mut tx_dns, rx_dns) = unbounded();
        let (to_bridge, fwd_rx) = unbounded();
        let (fwd_tx, from_bridge) = unbounded();
        let mut loopback = ManualLoopback::new(to_bridge, from_bridge);

        let bridge = Bridge::start(
            Config {
                message_timeout: Duration::from_secs(1),
            },
            Forwarder::manual(fwd_tx, fwd_rx),
            rx_dns,
        )
        .unwrap();

        // create a new mock client
        let mut client = MockClient::new(tx_dns).await;

        // send a message
        client.send_dns_msg(0xffff).await;

        // let it expire
        tokio::time::sleep(Duration::from_secs(5)).await;

        // loop it back, the response should be dropped
        loopback.loopback().await.unwrap();

        // check that no message arrives
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), client.receive_dns_msg()).await,
            Err(_)
        ));

        // check that messages can still be sent
        client.send_dns_msg(0xffff).await;
        loopback.loopback().await.unwrap();
        assert_eq!(client.receive_dns_msg().await.id(), 0xffff);

        bridge.shutdown().await.unwrap();
    }
}
