use anyhow::anyhow;
use futures::{
    channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender},
    Sink, SinkExt, Stream, StreamExt,
};
use serde::{de::Visitor, Deserialize};
use url::Url;

use super::DNSMessage;

pub mod tcp;
pub mod udp;

pub struct Forwarder {
    tx: UnboundedSender<DNSMessage>,
    rx: UnboundedReceiver<DNSMessage>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Address {
    Tcp(tcp::Config),
    Udp(udp::Config),
}

struct AddressVisitor;

impl<'de> Visitor<'de> for AddressVisitor {
    type Value = Address;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("string")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let url = Url::parse(v).map_err(|error| E::custom(error))?;
        let host = url
            .host()
            .ok_or_else(|| E::custom("missing host in url"))?
            .to_string();
        let port = url.port().unwrap_or(53);
        match url.scheme() {
            "tcp" => Ok(Address::Tcp(tcp::Config { host, port })),
            "udp" => Ok(Address::Udp(udp::Config { host, port })),
            rest => Err(E::custom(anyhow!("invalid url scheme '{}'", rest))),
        }
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(AddressVisitor)
    }
}

pub enum ForwarderTask {
    Tcp(tcp::Forwarder),
    Udp(udp::Forwarder),
}

impl ForwarderTask {

    pub async fn shutdown(&mut self) {
        match self {
            ForwarderTask::Tcp(task) => task.shutdown().await,
            ForwarderTask::Udp(task) => task.shutdown().await,
        }
    }

}

impl Forwarder {
    pub async fn try_new(config: Address) -> anyhow::Result<(ForwarderTask, Self)> {
        match config {
            Address::Tcp(tcp_config) => Self::tcp(tcp_config).await,
            Address::Udp(udp_config) => Self::udp(udp_config).await,
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
