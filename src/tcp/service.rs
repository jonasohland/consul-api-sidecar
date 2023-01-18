use std::{io, net::SocketAddr, path::Path};

use futures::future;
use rand::{distributions::Alphanumeric, Rng};
use tokio::net::{unix, TcpListener, TcpStream, UnixListener, UnixStream};

use anyhow::{Context, Result};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::Instrument;

use crate::task::{start_task, ShutdownHandle, TaskWrapper};

use super::{fwd::start_fwds, Address};

pub fn start_server(name: &str, path: String, addr: Address) -> TaskWrapper<()> {
    start_task(|shutdown| {
        run_server(shutdown, path, addr).instrument(tracing::error_span!("tcp-server", name))
    })
}

async fn start_server_services(path: String) -> Result<UnixListener> {
    if Path::new(path.as_str()).exists() {
        tokio::fs::remove_file(path.as_str())
            .await
            .context("failed to remove old socket file")?
    }
    UnixListener::bind(path.as_str()).context("failed to bind server socket")
}

enum ServerEvent {
    Shutdown,
    Accepted(io::Result<(UnixStream, unix::SocketAddr)>),
}

async fn run_unix_tcp_forwarding(
    shutdown: &mut ShutdownHandle,
    unix_stream: UnixStream,
    tcp_stream: TcpStream,
) {
    tracing::debug!("forwarder started");
    let (ra, wa) = tcp_stream.into_split();
    let (rb, wb) = unix_stream.into_split();
    let (mut fwd1, mut fwd2) = start_fwds(
        ra.compat(),
        rb.compat(),
        wa.compat_write(),
        wb.compat_write(),
    );
    tokio::select! {
        _ = fwd1.wait() => {}
        _ = fwd2.wait() => {}
        _ = shutdown => {}
    }
    fwd1.shutdown().await.ok();
    fwd2.shutdown().await.ok();
    tracing::debug!("forwarder done");
}

async fn run_server_session(
    mut shutdown: ShutdownHandle,
    unix_stream: UnixStream,
    address: Address,
) {
    tracing::debug!(?address, "connecting");
    if let Some(res) = tokio::select! {
        res = TcpStream::connect((address.host.clone(), address.port)) => {
            Some(res)
        }
        _ = &mut shutdown => {
            None
        }
    } {
        match res {
            Err(error) => {
                tracing::error!(?error, "connection failed")
            }
            Ok(tcp_stream) => {
                tracing::debug!(?address, "connected");
                run_unix_tcp_forwarding(&mut shutdown, unix_stream, tcp_stream).await;
            }
        }
    }
}

fn start_server_session(id: &str, stream: UnixStream, address: Address) -> TaskWrapper<()> {
    start_task(|shutdown| {
        run_server_session(shutdown, stream, address)
            .instrument(tracing::error_span!("tcp-fwd-session", id))
    })
}

async fn sessions_cleanup(sessions: &mut Vec<TaskWrapper<()>>) {
    if !sessions.is_empty() {
        let mut new_sessions = Vec::new();
        let mut opt_shutdown_sessions: Option<Vec<_>> = None;
        for session in sessions.drain(0..sessions.len()) {
            if session.is_done() {
                opt_shutdown_sessions
                    .get_or_insert_with(Default::default)
                    .push(session);
            } else {
                new_sessions.push(session);
            }
        }
        *sessions = new_sessions;
        if let Some(shutdown_sessions) = opt_shutdown_sessions {
            tracing::debug!(
                count = shutdown_sessions.len(),
                "cleaning up finished sessions"
            );
            future::join_all(shutdown_sessions.into_iter().map(|mut session| async move {
                session.shutdown().await.ok();
            }))
            .await;
        }
    }
}

fn random_id() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(7)
        .map(char::from)
        .collect()
}

async fn run_server(mut shutdown: ShutdownHandle, path: String, addr: Address) {
    match start_server_services(path).await {
        Err(error) => tracing::error!(?error, "failed to start service"),
        Ok(listener) => {
            let mut sessions = Vec::new();
            loop {
                match tokio::select! {
                    _ = &mut shutdown => {
                        ServerEvent::Shutdown
                    }
                    res = listener.accept() => {
                        ServerEvent::Accepted(res)
                    }
                } {
                    ServerEvent::Shutdown => break,
                    ServerEvent::Accepted(Err(error)) => {
                        tracing::error!(?error, "socket error");
                        break;
                    }
                    ServerEvent::Accepted(Ok((stream, _))) => {
                        sessions.push(start_server_session(&random_id(), stream, addr.clone()));
                    }
                }
                sessions_cleanup(&mut sessions).await;
            }
        }
    }
}

async fn run_client_session(mut shutdown: ShutdownHandle, tcp_stream: TcpStream, path: String) {
    tracing::debug!(?path, "connecting");
    if let Some(res) = tokio::select! {
        res = UnixStream::connect(path.as_str()) => Some(res),
        _ = &mut shutdown => None
    } {
        match res {
            Err(error) => tracing::error!(?error, "connection failed"),
            Ok(unix_stream) => {
                tracing::debug!(?path, "connected");
                run_unix_tcp_forwarding(&mut shutdown, unix_stream, tcp_stream).await;
            }
        }
    }
}

fn start_client_session(id: &str, stream: TcpStream, path: String) -> TaskWrapper<()> {
    start_task(|shutdown| {
        run_client_session(shutdown, stream, path)
            .instrument(tracing::error_span!("tcp-fwd-session", id))
    })
}

async fn run_client(mut shutdown: ShutdownHandle, path: String, listen: SocketAddr) {
    match TcpListener::bind(listen).await {
        Err(error) => tracing::error!(?error, "failed to bind listening tcp socket"),
        Ok(socket) => {
            tracing::debug!(address = ?listen, "listening");
            let mut sessions = Vec::new();
            loop {
                match tokio::select! {
                    _ = &mut shutdown => None,
                    res = socket.accept() => Some(res)
                } {
                    None => break,
                    Some(Err(error)) => {
                        tracing::error!(?error, "socket error");
                        break;
                    }
                    Some(Ok((stream, _))) => {
                        sessions.push(start_client_session(&random_id(), stream, path.clone()))
                    }
                }
                sessions_cleanup(&mut sessions).await;
            }
        }
    }
}

pub fn start_client(name: &str, path: String, listen: SocketAddr) -> TaskWrapper<()> {
    start_task(|shutdown| {
        run_client(shutdown, path, listen).instrument(tracing::error_span!("tcp-client", name))
    })
}
