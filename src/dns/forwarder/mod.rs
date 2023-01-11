use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    Sink, SinkExt, Stream, StreamExt,
};

use super::DNSMessage;

pub mod tcp;
pub mod udp;

pub struct Forwarder {
    tx: UnboundedSender<DNSMessage>,
    rx: UnboundedReceiver<DNSMessage>,
}

pub enum Config {
    Tcp(tcp::Config),
    Udp(udp::Config)
}

pub enum ForwarderTask {
    Tcp(tcp::Forwarder),
    Udp(udp::Forwarder),
}

impl Forwarder {

    pub async fn try_new(config: Config) -> anyhow::Result<(ForwarderTask, Self)> {
        match config {
            Config::Tcp(tcp_config) => Self::tcp(tcp_config).await,
            Config::Udp(udp_config) => Self::udp(udp_config).await,
        }
    }

    pub fn manual(tx: UnboundedSender<DNSMessage>, rx: UnboundedReceiver<DNSMessage>) -> Self {
        Self { tx, rx }
    }

    pub fn loopback() -> Self {
        let (tx, rx) = unbounded();
        Self { tx, rx }
    }

    pub async fn tcp(config: tcp::Config) -> anyhow::Result<(ForwarderTask, Self)> {
        let (tx, from_bridge) = unbounded();
        let (to_bridge, rx) = unbounded();
        Ok((
            ForwarderTask::Tcp(tcp::Forwarder::start(config, to_bridge, from_bridge).await),
            Forwarder { tx, rx },
        ))
    }

    pub async fn udp(config: udp::Config) -> anyhow::Result<(ForwarderTask, Self)> {
        let (tx, from_bridge) = unbounded();
        let (to_bridge, rx) = unbounded();
        Ok((
            ForwarderTask::Udp(udp::Forwarder::start(config, to_bridge, from_bridge).await?),
            Forwarder { tx, rx },
        ))
    }
}

impl Stream for Forwarder {
    type Item = DNSMessage;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_next_unpin(cx)
    }
}

impl Sink<DNSMessage> for Forwarder {
    type Error = <UnboundedSender<DNSMessage> as Sink<DNSMessage>>::Error;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.tx.poll_ready(cx)
    }

    fn start_send(mut self: std::pin::Pin<&mut Self>, item: DNSMessage) -> Result<(), Self::Error> {
        self.tx.start_send(item)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.tx.poll_flush_unpin(cx)
    }

    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.poll_close_unpin(cx)
    }
}
