use crate::task::{start_task, ShutdownHandle, TaskWrapper};
use bytes::BytesMut;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

async fn run_forwarder<R, W>(mut shutdown: ShutdownHandle, mut read: R, mut write: W)
where
    R: AsyncRead + Send + Unpin,
    W: AsyncWrite + Send + Unpin,
{
    let mut buf = BytesMut::zeroed(4096);
    loop {
        match tokio::select! {
            res = read.read(&mut buf) => Some(res),
            _ = &mut shutdown => None
        } {
            Some(Ok(len)) => {
                if len == 0 {
                    tracing::debug!("stream closed");
                    break;
                }
                if let Err(error) = write.write_all(buf.split_at(len).0).await {
                    tracing::error!(?error, "failed to write data");
                    break;
                }
            }
            Some(Err(error)) => {
                tracing::error!(?error, "failed to read data");
                break;
            }
            None => break,
        }
    }
}

pub fn start_fwds<ReadA, WriteA, ReadB, WriteB>(
    ra: ReadA,
    rb: ReadB,
    wa: WriteA,
    wb: WriteB,
) -> (TaskWrapper<()>, TaskWrapper<()>)
where
    ReadA: AsyncRead + Send + Unpin + 'static,
    ReadB: AsyncRead + Send + Unpin + 'static,
    WriteA: AsyncWrite + Send + Unpin + 'static,
    WriteB: AsyncWrite + Send + Unpin + 'static,
{
    (
        start_task(|shutdown| run_forwarder(shutdown, ra, wb)),
        start_task(|shutdown| run_forwarder(shutdown, rb, wa)),
    )
}
