use std::collections::HashMap;
use std::time::Duration;
use std::{io, net::SocketAddr};

use anyhow::Context;
use anyhow::Result;
use bytes::BytesMut;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};
use tokio::net::UdpSocket;
use tokio::time::Instant;
use tracing::Instrument;

use crate::task::{start_task, ShutdownHandle, TaskWrapper};

use super::DNSMessage;

pub struct Bridge {
    task: TaskWrapper<Result<()>>,
}

pub struct Config {
    pub listen: SocketAddr,
    pub timeout: Duration,
}

struct DNSMsgState {
    creation_time: Instant,
    source_address: SocketAddr,
}

impl DNSMsgState {
    fn new(addr: SocketAddr) -> Self {
        Self {
            creation_time: Instant::now(),
            source_address: addr,
        }
    }
}

enum BridgeEvent {
    Shutdown,
    Timeout,
    SocketError(io::Error),
    DNSFromNet(usize, SocketAddr),
    DNSFromBridge(DNSMessage),
}

impl Bridge {
    pub async fn shutdown(&mut self) {
        if let Ok(Err(error)) = self.task.shutdown().await {
            tracing::error!(?error, "client task shutdown failed");
        }
    }

    pub async fn start(
        config: Config,
        dns_tx: UnboundedSender<DNSMessage>,
        dns_rx: UnboundedReceiver<DNSMessage>,
    ) -> Result<Self> {
        let sock = UdpSocket::bind(config.listen)
            .await
            .context("failed to bind listening socket")?;
        Ok(Self {
            task: start_task(|shutdown| {
                Self::run(config, shutdown, sock, dns_tx, dns_rx)
                    .instrument(tracing::error_span!("dns-ingress-bridge"))
            }),
        })
    }

    fn cleanup(config: &Config, state: &mut HashMap<u16, DNSMsgState>) {
        let now = Instant::now();
        *state = state
            .drain()
            .filter(|(id, state)| {
                if now
                    .checked_duration_since(state.creation_time)
                    .map(|d| d < config.timeout)
                    .unwrap_or(false)
                {
                    // keep
                    true
                } else {
                    tracing::warn!(id, source = ?state.source_address, "remove expired request");
                    false
                }
            })
            .collect::<HashMap<_, _>>();
    }

    async fn run(
        config: Config,
        mut shutdown: ShutdownHandle,
        sock: UdpSocket,
        mut dns_tx: UnboundedSender<DNSMessage>,
        mut dns_rx: UnboundedReceiver<DNSMessage>,
    ) -> Result<()> {
        let mut buf = BytesMut::zeroed(1524);
        let mut mtx_state: HashMap<u16, DNSMsgState> = Default::default();
        loop {
            match tokio::select! {
                _ = &mut shutdown => BridgeEvent::Shutdown,
                _ = tokio::time::sleep(Duration::from_secs(3)) => BridgeEvent::Timeout,
                res = sock.recv_from(&mut buf) => match res {
                    Ok((len, addr)) => BridgeEvent::DNSFromNet(len, addr),
                    Err(e) => BridgeEvent::SocketError(e),
                },
                res = dns_rx.next() => match res {
                    Some(msg) => BridgeEvent::DNSFromBridge(msg),
                    None => BridgeEvent::Shutdown
                }
            } {
                BridgeEvent::Shutdown => {
                    tracing::debug!("shutting down");
                    break Ok(());
                }
                BridgeEvent::Timeout => {
                    // loop to run clean up
                }
                BridgeEvent::SocketError(error) => {
                    tracing::error!(
                        ?error,
                        "aborting because of unexpected error on listening DNS socket"
                    );
                    break Err(error).context("error on listening dns socket");
                }
                BridgeEvent::DNSFromBridge(msg) => match mtx_state.remove(&msg.id()) {
                    Some(state) => {
                        tracing::debug!(
                            id = msg.id(),
                            source = ?state.source_address,
                            "reply received for registered request"
                        );
                        if let Err(error) = sock.send_to(&msg.data, state.source_address).await {
                            tracing::warn!(?error, "failed to send dns reply")
                        }
                    }
                    None => {
                        tracing::warn!(
                            id = msg.id(),
                            "received dns reply from bridge for unknown receiver"
                        )
                    }
                },
                BridgeEvent::DNSFromNet(len, addr) => {
                    match DNSMessage::try_new(BytesMut::from(&buf[0..len])) {
                        Ok(msg) => {
                            let id = msg.id();
                            if let Err(error) = dns_tx.send(msg).await {
                                tracing::warn!(?error, "failed to send dns message to bridge")
                            }
                            tracing::debug!(id, source = ?addr, "register new DNS request");
                            if let Some(prev) = mtx_state.insert(id, DNSMsgState::new(addr)) {
                                tracing::warn!(id, "dns msg id collision");
                                mtx_state.insert(id, prev);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, %addr, "received invalid DNS message");
                        }
                    }
                }
            }
            Self::cleanup(&config, &mut mtx_state);
        }
    }
}

#[allow(unused)]
mod test {
    use std::net::{IpAddr, Ipv4Addr};

    use futures::channel::mpsc::unbounded;
    use tokio::net::UdpSocket;

    use crate::testing::make_bound_socket;

    use super::*;

    #[tokio::test]
    async fn test() {
        let port = {
            let (_, port) = make_bound_socket("127.0.0.1").await.unwrap();
            port
        };

        // loopback channel
        let (tx, rx) = unbounded();
        // launch a new bridge task
        let mut bridge = Bridge::start(
            Config {
                listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
                timeout: Duration::from_secs(100),
            },
            tx,
            rx,
        )
        .await
        .unwrap();

        // send fake DNS request
        let sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        sock.send_to(&[0x31, 0x4f], format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        // receive DNS reply
        let mut buf = BytesMut::zeroed(2);
        assert_eq!(sock.recv(&mut buf).await.unwrap(), 2);
        assert_eq!(DNSMessage::try_new(buf).unwrap().id(), 0x314f);

        // shut down
        bridge.shutdown().await;
    }

    #[tokio::test]
    async fn timeout() {
        // create channels to and from bridge
        let (tx, mut from_bridge) = unbounded();
        let (mut to_bridge, rx) = unbounded();

        // get port that is likely unused
        let port = {
            let (_, port) = make_bound_socket("127.0.0.1").await.unwrap();
            port
        };

        // create a new bridge
        let mut bridge = Bridge::start(
            Config {
                listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
                timeout: Duration::from_secs(1),
            },
            tx,
            rx,
        )
        .await
        .unwrap();

        // send a fake dns request
        let sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        sock.send_to(&[0x31, 0x4f], format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        // receive message
        let msg = from_bridge.next().await.unwrap();

        // wait until the bridge should have timed out the message
        tokio::time::sleep(Duration::from_secs(5)).await;

        // send the reply to the bridge
        to_bridge.send(msg).await.unwrap();

        // check that no message will be delivered after timeout
        if tokio::time::timeout(Duration::from_secs(3), sock.recv_from(&mut [0u8]))
            .await
            .is_ok()
        {
            panic!("expected to receive no message after timeout")
        }

        // shut down
        bridge.shutdown().await;
    }

    #[tokio::test]
    async fn collision() {
        let port = {
            let (_, port) = make_bound_socket("127.0.0.1").await.unwrap();
            port
        };

        // create channels to and from bridge
        let (tx, mut from_bridge) = unbounded();
        let (mut to_bridge, rx) = unbounded();

        // launch a new bridge task
        let mut bridge = Bridge::start(
            Config {
                listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port),
                timeout: Duration::from_secs(100),
            },
            tx,
            rx,
        )
        .await
        .unwrap();

        // send fake DNS request
        let sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        sock.send_to(&[0x31, 0x4f], format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        // send a second message whose ID collides with the previous one
        let sock2 = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        sock2
            .send_to(&[0x31, 0x4f], format!("127.0.0.1:{}", port))
            .await
            .unwrap();

        // loopback 1 message
        to_bridge
            .send(from_bridge.next().await.unwrap())
            .await
            .unwrap();

        // receive DNS reply
        let mut buf = BytesMut::zeroed(2);
        assert_eq!(sock.recv(&mut buf).await.unwrap(), 2);
        assert_eq!(DNSMessage::try_new(buf).unwrap().id(), 0x314f);

        // shut down
        bridge.shutdown().await;
    }
}
