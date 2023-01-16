use std::mem::replace;
use std::{io, path::Path, time::Duration};

use anyhow::{Context, Result};
use futures::channel::mpsc::unbounded;
use rand::{distributions::Alphanumeric, Rng};
use tokio::net::{unix::SocketAddr, UnixListener, UnixStream};
use tokio_util::compat::TokioAsyncReadCompatExt;
use tracing::Instrument;

use crate::dns;
use crate::task::{start_task, ShutdownHandle, TaskWrapper};

use super::ingress_bridge;
use super::{
    egress_bridge::{self},
    forwarder::{self, ForwarderTask},
};

pub fn start_server(
    name: &str,
    path: String,
    address: forwarder::Address,
    timeout: u64,
) -> TaskWrapper<()> {
    start_task(|shutdown| {
        run_server(shutdown, path, address, timeout)
            .instrument(tracing::error_span!("dns-server", name))
    })
}

async fn start_server_services(
    path: &str,
    address: forwarder::Address,
    timeout: u64,
) -> Result<(
    egress_bridge::Bridge,
    dns::server::Server,
    ForwarderTask,
    UnixListener,
)> {
    // remove old socket if exists
    if Path::new(path).exists() {
        tokio::fs::remove_file(path)
            .await
            .context("failed to remove old socket file")?
    }

    let listener = UnixListener::bind(path).context("failed to bind unix socket")?;

    let (fwd_task, fwd) = match address {
        forwarder::Address::Tcp(addr) => forwarder::Forwarder::tcp(addr).await?,
        forwarder::Address::Udp(addr) => forwarder::Forwarder::udp(addr).await?,
    };

    let (egress_bridge_tx, egress_bridge_rx) = unbounded();
    let bridge = egress_bridge::Bridge::start(
        egress_bridge::Config {
            message_timeout: Duration::from_secs(timeout),
        },
        fwd,
        egress_bridge_rx,
    )
    .context("failed to start egress bridge")?;

    Ok((
        bridge,
        dns::server::Server::new(egress_bridge_tx),
        fwd_task,
        listener,
    ))
}

enum ServerEvent {
    Shutdown,
    Accepted(io::Result<(UnixStream, SocketAddr)>),
}

fn random_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(7)
        .map(char::from)
        .collect()
}

async fn run_server(
    mut shutdown: ShutdownHandle,
    path: String,
    address: forwarder::Address,
    timeout: u64,
) {
    match start_server_services(&path, address, timeout).await {
        Err(error) => tracing::error!(?error, "service startup failed"),
        Ok((mut bridge, mut server, mut fwd, listener)) => {
            loop {
                match tokio::select! {
                    _ = &mut shutdown => {
                        ServerEvent::Shutdown
                    }
                    res = listener.accept() => {
                        ServerEvent::Accepted(res)
                    }
                } {
                    ServerEvent::Shutdown => {
                        tracing::debug!("server shut down");
                        break;
                    }
                    ServerEvent::Accepted(res) => match res {
                        Ok((stream, _)) => {
                            server.accept(&random_id(), stream.compat()).await;
                        }
                        Err(error) => {
                            tracing::error!(?error, "shutting down because of socket error");
                            break;
                        }
                    },
                }
            }
            server.shutdown().await;
            bridge.shutdown().await;
            fwd.shutdown().await;
        }
    }
}

pub struct ClientConfig {}

async fn start_client_services(
    name: &str,
    path: String,
    listen: std::net::SocketAddr,
    timeout: u64,
) -> Result<(ingress_bridge::Bridge, dns::client::Client)> {
    let (to_bridge, from_ch) = unbounded();
    let (to_ch, from_bridge) = unbounded();

    let sock = UnixStream::connect(&path)
        .await
        .context("failed to connect to server socket")?;

    let bridge = ingress_bridge::Bridge::start(
        ingress_bridge::Config {
            listen,
            timeout: Duration::from_secs(timeout),
        },
        to_bridge,
        from_bridge,
    )
    .await
    .context("failed to start ingress bridge")?;

    let client = dns::client::Client::start(name, sock.compat(), to_ch, from_ch);
    tracing::debug!("client running");
    Ok((bridge, client))
}

enum ClientState {
    Starting,
    Running {
        bridge: ingress_bridge::Bridge,
        client: dns::client::Client,
    },
    Invalid,
}

pub fn start_client(
    name: &str,
    path: String,
    listen: std::net::SocketAddr,
    timeout: u64,
) -> TaskWrapper<()> {
    start_task(|shutdown| {
        run_client(shutdown, name.to_owned(), path, listen, timeout)
            .instrument(tracing::error_span!("dns-client", name))
    })
}

async fn run_client(
    mut shutdown: ShutdownHandle,
    name: String,
    path: String,
    listen: std::net::SocketAddr,
    timeout: u64,
) {
    let mut state = ClientState::Starting;
    loop {
        match replace(&mut state, ClientState::Invalid) {
            ClientState::Starting => {
                match start_client_services(name.as_str(), path.clone(), listen, timeout).await {
                    Ok((bridge, client)) => state = ClientState::Running { bridge, client },
                    Err(error) => {
                        tracing::error!(?error, "client startup failed");
                        match tokio::time::timeout(Duration::from_secs(5), &mut shutdown).await {
                            Err(_) => state = ClientState::Starting,
                            Ok(_) => break,
                        }
                    }
                }
            }
            ClientState::Running {
                mut bridge,
                mut client,
            } => match tokio::time::timeout(Duration::from_secs(5), &mut shutdown).await {
                Err(_) => {
                    if client.is_done() {
                        tracing::debug!("client exited, restarting");
                        bridge.shutdown().await;
                        state = ClientState::Starting;
                    } else {
                        state = ClientState::Running { bridge, client };
                    }
                }
                Ok(_) => {
                    client.shutdown().await;
                    bridge.shutdown().await;
                    break;
                }
            },
            ClientState::Invalid => todo!(),
        }
    }
}
