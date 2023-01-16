use std::time::Duration;

use consul_api_sidecar::dns::{
    forwarder::udp::{Config as FwdConfig, Forwarder},
    ingress_bridge::{Bridge, Config as BridgeConfig},
};
use futures::channel::mpsc::unbounded;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .init();

    let (to_net, from_bridge) = unbounded();
    let (to_bridge, from_net) = unbounded();

    let mut forwarder = Forwarder::start(
        FwdConfig {
            host: "localhost".to_owned(),
            port: 8600,
        },
        to_bridge,
        from_bridge,
    )
    .await
    .unwrap();

    let mut bridge = Bridge::start(
        BridgeConfig {
            listen: "127.0.0.1:5355".parse().unwrap(),
            timeout: Duration::from_secs(100),
        },
        to_net,
        from_net,
    )
    .await
    .unwrap();

    tokio::signal::ctrl_c().await.unwrap();

    bridge.shutdown().await;
    forwarder.shutdown().await;
}
