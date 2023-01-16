use std::{io, mem::replace, time::Duration};

use anyhow::Result;
use futures::{
    channel::mpsc::{UnboundedReceiver, UnboundedSender},
    SinkExt, StreamExt,
};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::Instrument;

use crate::{
    dns::{receive_dns_message, send_dns_message, DNSMessage},
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

enum ForwarderState {
    Connected(TcpStream),
    Waiting,
    Connecting,
    Invalid,
}

enum ForwarderEventConnected {
    DnsFromNet(Result<DNSMessage>),
    DnsFromBridge(DNSMessage),
    Shutdown,
}

enum ForwarderEventNotConnected {
    Connected(TcpStream),
    Shutdown,
}

enum ForwarderEventWaiting {
    ConnectNow,
    Shutdown,
}

impl Forwarder {
    pub async fn start(
        config: Config,
        to_bridge: UnboundedSender<DNSMessage>,
        from_bridge: UnboundedReceiver<DNSMessage>,
    ) -> Self {
        Self {
            task: start_task(|shutdown| {
                Self::run(config, shutdown, to_bridge, from_bridge)
                    .instrument(tracing::error_span!("dns-fwd-tcp"))
            }),
        }
    }

    pub async fn shutdown(&mut self) {
        if let Ok(Err(error)) = self.task.shutdown().await {
            tracing::error!(?error, "forwarder task failed");
        }
    }

    async fn try_connect(conf: &Config) -> TcpStream {
        loop {
            match TcpStream::connect((conf.host.clone(), conf.port)).await {
                Ok(s) => break s,
                Err(error) => {
                    tracing::warn!(?error, "connection failed");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn run(
        config: Config,
        mut shutdown: ShutdownHandle,
        mut to_bridge: UnboundedSender<DNSMessage>,
        from_bridge: UnboundedReceiver<DNSMessage>,
    ) -> Result<()> {
        let mut connect_state = ForwarderState::Waiting;
        let mut from_bridge = from_bridge.peekable();
        loop {
            match replace(&mut connect_state, ForwarderState::Invalid) {
                ForwarderState::Waiting => {
                    match tokio::select! {
                        opt = Pin::new(&mut from_bridge).peek() => {
                            match opt {
                                Some(_) => ForwarderEventWaiting::ConnectNow,
                                None => ForwarderEventWaiting::Shutdown
                            }
                        }
                        _ = &mut shutdown => { ForwarderEventWaiting::Shutdown }
                    } {
                        ForwarderEventWaiting::ConnectNow => {
                            connect_state = ForwarderState::Connecting
                        }
                        ForwarderEventWaiting::Shutdown => break Ok(()),
                    }
                }
                ForwarderState::Connecting => {
                    match tokio::select! {
                        _ = &mut shutdown => { ForwarderEventNotConnected::Shutdown }
                        stream = Self::try_connect(&config) => { ForwarderEventNotConnected::Connected(stream) }
                    } {
                        ForwarderEventNotConnected::Connected(stream) => {
                            tracing::debug!("connected");
                            connect_state = ForwarderState::Connected(stream)
                        }
                        ForwarderEventNotConnected::Shutdown => break Ok(()),
                    }
                }
                ForwarderState::Connected(stream) => {
                    let mut stream = stream.compat();
                    match tokio::select! {
                        dns = receive_dns_message(&mut stream) => {
                            ForwarderEventConnected::DnsFromNet(dns)
                        }
                        opt_msg = &mut from_bridge.next() => {
                            match opt_msg {
                                Some(msg) => ForwarderEventConnected::DnsFromBridge(msg),
                                None => ForwarderEventConnected::Shutdown,
                            }
                        }
                        _ = &mut shutdown => {
                            ForwarderEventConnected::Shutdown
                        }
                    } {
                        ForwarderEventConnected::DnsFromNet(res) => match res {
                            Ok(msg) => {
                                if to_bridge.send(msg).await.is_err() {
                                    break Ok(());
                                }
                                connect_state = ForwarderState::Connected(stream.into_inner())
                            }
                            Err(error) => {
                                match error.downcast_ref::<io::Error>() {
                                    Some(ioe) if ioe.kind() == io::ErrorKind::UnexpectedEof => {
                                        tracing::debug!("disconnected")
                                    }
                                    _ => {
                                        tracing::warn!(?error, "dns/tcp stream error")
                                    }
                                };
                                connect_state = ForwarderState::Waiting
                            }
                        },
                        ForwarderEventConnected::DnsFromBridge(dns) => {
                            match send_dns_message(&mut stream, &dns).await {
                                Ok(_) => {
                                    connect_state = ForwarderState::Connected(stream.into_inner())
                                }
                                Err(error) => {
                                    tracing::warn!(?error, "dns/tcp stream error");
                                    connect_state = ForwarderState::Waiting
                                }
                            }
                        }
                        ForwarderEventConnected::Shutdown => break Ok(()),
                    }
                }
                ForwarderState::Invalid => unreachable!(),
            }
        }
    }
}
