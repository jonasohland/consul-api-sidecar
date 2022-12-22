use std::{
    io::{self, IoSlice},
    net::{IpAddr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::anyhow;
use bytes::{BufMut, BytesMut};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::{AsyncRead, AsyncWrite, StreamExt};
use rand::Rng;
use tokio::net::UdpSocket;

pub struct TestConnection {
    tx: UnboundedSender<Vec<u8>>,
    rx: UnboundedReceiver<Vec<u8>>,
    rx_buf: BytesMut,
}

pub async fn make_bound_socket(host: &str) -> anyhow::Result<(UdpSocket, u16)> {
    let ip: IpAddr = host.parse()?;
    for _ in 0..10000 {
        let candidate = rand::thread_rng().gen_range(20000u16..40000u16);
        if let Ok(sock) = UdpSocket::bind(SocketAddr::new(ip, candidate)).await {
            return Ok((sock, candidate));
        }
    }
    Err(anyhow!("failed to find a free udp port"))
}

impl AsyncRead for TestConnection {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.rx_buf.len() {
            0 => match this.rx.poll_next_unpin(cx) {
                Poll::Ready(Some(x)) => match x.len() {
                    len if len <= buf.len() => {
                        buf.split_at_mut(x.len()).0.copy_from_slice(&x);
                        Poll::Ready(Ok(x.len()))
                    }
                    _ => {
                        let (now, later) = x.split_at(buf.len());
                        buf.copy_from_slice(now);
                        this.rx_buf.put(later);
                        Poll::Ready(Ok(buf.len()))
                    }
                },
                Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "underlying channel closed",
                ))),
                Poll::Pending => Poll::Pending,
            },
            len if len <= buf.len() => {
                buf.split_at_mut(len).0.copy_from_slice(&this.rx_buf);
                Poll::Ready(Ok(len))
            }
            _ => {
                buf.copy_from_slice(&this.rx_buf.split_to(buf.len()));
                Poll::Ready(Ok(buf.len()))
            }
        }
    }
}

impl AsyncWrite for TestConnection {
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        for b in bufs {
            if !b.is_empty() {
                return self.poll_write(cx, b);
            }
        }
        Poll::Ready(Ok(0))
    }

    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.tx.unbounded_send(Vec::from(buf)) {
            Ok(_) => Poll::Ready(Ok(buf.len())),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "underlying channel closed",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

pub fn make_test_connection() -> (TestConnection, TestConnection) {
    let (tx0, rx0) = unbounded();
    let (tx1, rx1) = unbounded();
    (
        TestConnection {
            tx: tx0,
            rx: rx1,
            rx_buf: Default::default(),
        },
        TestConnection {
            tx: tx1,
            rx: rx0,
            rx_buf: Default::default(),
        },
    )
}

#[allow(unused)]
mod test {
    use super::make_test_connection;
    use bytes::{Bytes, BytesMut};
    use futures::{AsyncReadExt, AsyncWriteExt};
    use std::str::from_utf8;

    #[tokio::test]
    async fn basic_rw() {
        let (mut a, mut b) = make_test_connection();
        assert_eq!(a.write(b"Hello").await.unwrap(), 5);
        let mut bytes = BytesMut::zeroed(5);
        assert_eq!(b.read(&mut bytes).await.unwrap(), 5);
        assert_eq!(bytes, Bytes::from_static(b"Hello"));
    }

    #[tokio::test]
    async fn buffered_rw0() {
        let (mut a, mut b) = make_test_connection();
        assert_eq!(a.write(b"012345").await.unwrap(), 6);
        let mut rx0 = BytesMut::zeroed(3);
        let mut rx1 = BytesMut::zeroed(3);
        assert_eq!(b.read(&mut rx0).await.unwrap(), 3);
        assert_eq!(b.read(&mut rx1).await.unwrap(), 3);
        assert_eq!(rx0, Bytes::from_static(b"012"));
        assert_eq!(rx1, Bytes::from_static(b"345"));
    }

    #[tokio::test]
    async fn buffered_rw1() {
        let (mut a, mut b) = make_test_connection();
        assert_eq!(a.write(b"012345678").await.unwrap(), 9);
        let mut rx0 = BytesMut::zeroed(3);
        let mut rx1 = BytesMut::zeroed(3);
        let mut rx2 = BytesMut::zeroed(3);
        assert_eq!(b.read(&mut rx0).await.unwrap(), 3);
        assert_eq!(b.read(&mut rx1).await.unwrap(), 3);
        assert_eq!(b.read(&mut rx2).await.unwrap(), 3);
        assert_eq!(rx0, Bytes::from_static(b"012"));
        assert_eq!(rx1, Bytes::from_static(b"345"));
        assert_eq!(rx2, Bytes::from_static(b"678"));
    }
}
