use std::{io, net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use bytes::BytesMut;
use futures::{
    channel::mpsc::{UnboundedReceiver, UnboundedSender},
    SinkExt, StreamExt,
};
use tokio::{
    net::{lookup_host, UdpSocket},
    time::Instant,
};
use tracing::Instrument;

use crate::{
    dns::DNSMessage,
    task::{start_task, ShutdownHandle, TaskWrapper},
};

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    pub host: String,
    pub port: u16,
}

pub struct Forwarder {
    task: TaskWrapper<Result<()>>,
}

pub enum ForwarderEvent {
    Shutdown,
    SocketError(io::Error),
    DNSFromBridge(DNSMessage),
    DNSFromNet(usize, SocketAddr),
}

enum LookupEvent {
    Shutdown,
    LookupResult(io::Result<Vec<SocketAddr>>),
}

impl Forwarder {
    pub async fn shutdown(&mut self) {
        if let Ok(Err(error)) = self.task.shutdown().await {
            tracing::error!(?error, "forwarder task failed");
        }
    }

    pub async fn start(
        config: Config,
        to_bridge: UnboundedSender<DNSMessage>,
        from_bridge: UnboundedReceiver<DNSMessage>,
    ) -> Result<Self> {
        let sock = UdpSocket::bind("0.0.0.0:0").await?;
        Ok(Self {
            task: start_task(|shutdown| {
                Self::run(config, sock, shutdown, to_bridge, from_bridge)
                    .instrument(tracing::error_span!("dns-fwd-udp"))
            }),
        })
    }

    async fn run(
        config: Config,
        sock: UdpSocket,
        mut shutdown: ShutdownHandle,
        mut to_bridge: UnboundedSender<DNSMessage>,
        mut from_bridge: UnboundedReceiver<DNSMessage>,
    ) -> Result<()> {
        match loop {
            let begin = Instant::now();
            match tokio::select! {
                res = lookup_host((config.host.as_str(), config.port)) => {
                    LookupEvent::LookupResult(res.map(|it| it.filter(|addr| addr.ip().is_ipv4()).collect::<Vec<_>>()))
                }
                _ = &mut shutdown => {
                    LookupEvent::Shutdown
                }
            } {
                LookupEvent::Shutdown => break None,
                LookupEvent::LookupResult(Ok(hosts)) => {
                    if !hosts.is_empty() {
                        tracing::debug!(?hosts, "dns resolution succeeded");
                        break Some(hosts);
                    } else {
                        tracing::warn!(host = config.host, "dns lookup returned no results")
                    }
                }
                LookupEvent::LookupResult(Err(err)) => {
                    tracing::warn!(host = config.host, error = ?err, "dns lookup failed")
                }
            }
            let sleep_time = Duration::from_secs(5)
                .checked_sub(
                    Instant::now()
                        .checked_duration_since(begin)
                        .unwrap_or_else(|| Duration::from_secs(0)),
                )
                .unwrap_or_else(|| Duration::from_secs(5));
            tracing::trace!(?sleep_time, "sleep before next dns resolution attempt");
            if tokio::select! {
                _ = tokio::time::sleep(sleep_time) => {
                    false
                }
                _ = &mut shutdown => {
                    true
                }
            } {
                break None;
            }
        } {
            None => Ok(()),
            Some(hosts) => {
                let mut buf = BytesMut::zeroed(2048);
                loop {
                    match tokio::select! {
                        _ = &mut shutdown => { ForwarderEvent::Shutdown }
                        res = from_bridge.next() => {
                            match res {
                                Some(msg) => ForwarderEvent::DNSFromBridge(msg),
                                None => ForwarderEvent::Shutdown,
                            }
                        }
                        res = sock.recv_from(&mut buf) => {
                            match res {
                                Err(error) => ForwarderEvent::SocketError(error),
                                Ok((len, addr)) => ForwarderEvent::DNSFromNet(len, addr)
                            }
                        }
                    } {
                        ForwarderEvent::Shutdown => break Ok(()),
                        ForwarderEvent::SocketError(error) => {
                            break Err(error).context("error on dns socket");
                        }
                        ForwarderEvent::DNSFromBridge(msg) => {
                            for host in &hosts {
                                if let Err(error) = sock.send_to(&msg.data, host).await {
                                    tracing::trace!(%host, ?error, "failed to send dns message")
                                }
                            }
                        }
                        ForwarderEvent::DNSFromNet(len, addr) => {
                            match DNSMessage::try_new(BytesMut::from(buf.split_at(len).0)) {
                                Ok(msg) => {
                                    let id = msg.id();
                                    if let Err(error) = to_bridge.send(msg).await {
                                        tracing::warn!(
                                            id,
                                            %addr,
                                            ?error,
                                            "failed to forward received dns message to bridge"
                                        );
                                    }
                                }
                                Err(error) => {
                                    tracing::trace!(?error, %addr, "received invalid dns message");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
