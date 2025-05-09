use anyhow::Result;
use bytes::BytesMut;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod client;
pub mod egress_bridge;
pub mod forwarder;
pub mod ingress_bridge;
pub mod server;
pub mod service;

pub struct DNSMessage {
    data: BytesMut,
}

pub struct DNSMessageReader {
    current_msg_len: u16,
}

impl DNSMessageReader {
    fn new() -> DNSMessageReader {
        return DNSMessageReader { current_msg_len: 0 };
    }

    async fn read<S>(&mut self, mut s: S) -> Result<DNSMessage>
    where
        S: AsyncRead + Unpin,
    {
        if self.current_msg_len == 0 {
            let mut len_buf = [0u8; 2];
            s.read_exact(&mut len_buf).await?;

            self.current_msg_len = u16::from_be_bytes(len_buf)
        }

        let mut data = BytesMut::zeroed(self.current_msg_len as usize);
        s.read_exact(&mut data).await?;

        self.current_msg_len = 0;

        DNSMessage::try_new(data)
    }
}

impl DNSMessage {
    pub fn try_new(data: BytesMut) -> Result<Self> {
        if data.len() >= 2 {
            Ok(Self { data })
        } else {
            Err(anyhow::anyhow!("not enough data"))
        }
    }

    pub fn id(&self) -> u16 {
        u16::from_be_bytes(self.data[0..2].try_into().unwrap())
    }
}

pub async fn send_dns_message<S>(s: &mut S, msg: &DNSMessage) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    s.write_all(&(msg.data.len() as u16).to_be_bytes()).await?;
    s.write_all(&msg.data).await?;
    Ok(())
}

#[allow(unused)]
mod test {
    use bytes::BytesMut;
    use tokio::task;

    use crate::testing::make_test_connection;

    use super::{send_dns_message, DNSMessage, DNSMessageReader};

    #[test]
    pub fn make_msg() {
        let msg = DNSMessage::try_new(BytesMut::from([0x31, 0xa3].as_slice())).unwrap();
        assert_eq!(msg.id(), 0x31a3)
    }

    #[tokio::test]
    async fn rx_tx() {
        let (mut a, mut b) = make_test_connection();

        let ta = task::spawn(async move {
            let mut reader = DNSMessageReader::new();
            let message = reader.read(&mut a).await.unwrap();
            assert_eq!(message.id(), 0xff43);
        });

        let tb = task::spawn(async move {
            send_dns_message(
                &mut b,
                &DNSMessage::try_new(BytesMut::from([0xff, 0x43].as_slice())).unwrap(),
            )
            .await
            .unwrap();
        });

        ta.await.unwrap();
        tb.await.unwrap();
    }
}
