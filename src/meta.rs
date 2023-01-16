use std::time::Duration;

use anyhow::Result;
use futures::{AsyncRead, AsyncWrite};

use crate::handshake::Frame;

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct ServerMeta {
    pub name: String,
}

impl ServerMeta {
    pub async fn read_from_stream<S>(s: &mut S) -> Result<ServerMeta>
    where
        S: AsyncRead + Unpin,
    {
        Frame::read_from(s, Duration::from_secs(5))
            .await?
            .unmarshal()
    }

    pub async fn write_to_stream<S>(&self, s: &mut S) -> Result<()>
    where
        S: AsyncWrite + Unpin,
    {
        Frame::marshal(self)?
            .write_to(s, Duration::from_secs(5))
            .await
    }
}

#[derive(serde::Deserialize, serde::Serialize, Debug)]
pub struct ClientMeta {
    pub name: String,
}
 
impl ClientMeta {
    pub async fn read_from_stream<S>(s: &mut S) -> Result<ClientMeta>
    where
        S: AsyncRead + Unpin,
    {
        Frame::read_from(s, Duration::from_secs(5))
            .await?
            .unmarshal()
    }

    pub async fn write_to_stream<S>(&self, s: &mut S) -> Result<()>
    where
        S: AsyncWrite + Unpin,
    {
        Frame::marshal(self)?
            .write_to(s, Duration::from_secs(5))
            .await
    }
}