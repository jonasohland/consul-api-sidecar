use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct Frame {
    data: Vec<u8>,
}

const MSG_LEN_MAX: usize = 0x400000;

impl Frame {
    pub fn marshal<T>(message: &T) -> Result<Frame>
    where
        T: serde::ser::Serialize,
    {
        Ok(Self {
            data: serde_json::ser::to_vec(message)?,
        })
    }

    pub fn unmarshal<'de, T>(&'de self) -> Result<T>
    where
        T: serde::Deserialize<'de>,
    {
        Ok(serde_json::de::from_slice::<T>(&self.data)?)
    }

    pub async fn read_from<S>(s: &mut S, timeout: Duration) -> Result<Frame>
    where
        S: AsyncRead + Unpin,
    {
        tokio::time::timeout(timeout, async move {
            let mut msg_len_buf = [0u8; 4];
            s.read_exact(&mut msg_len_buf).await?;
            let len = u32::from_be_bytes(msg_len_buf) as usize;
            if len > MSG_LEN_MAX {
                Err(anyhow!(
                    "message length invalid, was: {}, must be < {}",
                    len,
                    MSG_LEN_MAX
                ))
            } else {
                let mut data = vec![0u8; len];
                tracing::trace!(len, "receive frame");
                s.read_exact(&mut data).await?;
                Ok(Frame { data })
            }
        })
        .await?
    }

    pub async fn write_to<S>(&self, s: &mut S, timeout: Duration) -> Result<()>
    where
        S: AsyncWrite + Unpin,
    {
        tokio::time::timeout(timeout, async move {
            let len_buf = (self.data.len() as u32).to_be_bytes();
            tracing::trace!(len = self.data.len(), "send frame");
            s.write_all(&len_buf).await?;
            s.write_all(&self.data).await?;
            Ok(())
        })
        .await?
    }
}
