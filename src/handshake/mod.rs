use std::{fmt::Display, net::IpAddr, time::Duration};

use self::frame::Frame;
use anyhow::{anyhow, Context, Result};
use futures::{AsyncRead, AsyncWrite};

pub mod frame;

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug)]
pub struct ChannelError {
    reason: String,
    code: u32,
}

impl Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "channel negotiation failed: {}, code: {}",
            self.reason, self.code
        )
    }
}

impl std::error::Error for ChannelError {}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone, PartialEq, Eq)]
pub enum ChannelAddress {
    IpAddressAndPort { address: IpAddr, port: u16 },
    Virtual(String),
}

impl Display for ChannelAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelAddress::IpAddressAndPort { address, port } => {
                write!(f, "ip:{}:{}", address, port)
            }
            ChannelAddress::Virtual(addr) => write!(f, "virtual:{}", addr),
        }
    }
}

pub trait ChannelAcceptor: Send + Sync {
    fn accept(
        &mut self,
        ty: &str,
        requested_address: &ChannelAddress,
        offer: &ChannelOffer,
    ) -> Result<(), ChannelError>;
}

pub trait ChannelSelector: Send + Sync {
    type Selection;
    fn select(
        &mut self,
        ty: &str,
        address: &ChannelAddress,
    ) -> Result<(ChannelOffer, Self::Selection), ChannelError>;
}

#[derive(serde::Deserialize, serde::Serialize)]
struct RequestMessage {
    #[serde(rename = "type")]
    ty: String,

    address: ChannelAddress,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ChannelOffer {
    address: ChannelAddress,
    info: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize)]
enum OfferMessage {
    #[serde(rename = "offer")]
    Offer {
        #[serde(rename = "type")]
        ty: String,

        offer: ChannelOffer,
    },
    #[serde(rename = "deny")]
    Deny {
        #[serde(rename = "type")]
        ty: String,

        detail: ChannelError,
    },
}

#[derive(serde::Deserialize, serde::Serialize)]
enum AcceptMessage {
    #[serde(rename = "accept")]
    Accept {
        #[serde(rename = "type")]
        ty: String,
    },

    #[serde(rename = "reject")]
    Reject {
        #[serde(rename = "type")]
        ty: String,

        detail: ChannelError,
    },
}

enum CallerHandshakeState {
    Initial { ty: String, address: ChannelAddress },
    WaitOffer { ty: String, address: ChannelAddress },
    Ended,
    Invalid,
}

pub struct CallerHandshake<Acceptor>
where
    Acceptor: ChannelAcceptor,
{
    state: CallerHandshakeState,
    acceptor: Acceptor,
}

impl<Acceptor> CallerHandshake<Acceptor>
where
    Acceptor: ChannelAcceptor,
{
    #[allow(clippy::new_without_default)]
    pub fn new(ty: String, address: ChannelAddress, acceptor: Acceptor) -> Self {
        Self {
            state: CallerHandshakeState::Initial { ty, address },
            acceptor,
        }
    }

    pub async fn perform<S>(&mut self, sock: &mut S) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            match std::mem::replace(&mut self.state, CallerHandshakeState::Invalid) {
                CallerHandshakeState::Initial { ty, address } => {
                    let req = RequestMessage { ty, address };
                    // send the initial request frame
                    Frame::marshal(&req)?
                        .write_to(sock, Duration::from_secs(5))
                        .await
                        .context("send request frame")?;
                    tracing::debug!(
                        ty = req.ty,
                        address = %req.address,
                        "sent channel request frame, wait for channel offer"
                    );
                    self.state = CallerHandshakeState::WaitOffer {
                        ty: req.ty,
                        address: req.address,
                    };
                }
                CallerHandshakeState::WaitOffer { ty, address } => {
                    let offer_msg: OfferMessage = Frame::read_from(sock, Duration::from_secs(5))
                        .await
                        .context("receive offer frame")?
                        .unmarshal()?;
                    match offer_msg {
                        OfferMessage::Offer {
                            ty: offer_ty,
                            offer,
                        } => {
                            if ty != offer_ty {
                                self.state = CallerHandshakeState::Ended;
                                break Err(anyhow!("offer contained wrong type"));
                            } else {
                                match self.acceptor.accept(&ty, &address, &offer) {
                                    Ok(_) => {
                                        // send ACK message
                                        tracing::debug!(ty, "accepted channel offer, send ACK");
                                        Frame::marshal(&AcceptMessage::Accept { ty })?
                                            .write_to(sock, Duration::from_secs(5))
                                            .await
                                            .context("send ACK frame")?;
                                        self.state = CallerHandshakeState::Ended;
                                        break Ok(());
                                    }
                                    Err(err) => {
                                        self.state = CallerHandshakeState::Ended;
                                        Frame::marshal(&AcceptMessage::Reject {
                                            ty,
                                            detail: err.clone(),
                                        })?
                                        .write_to(sock, Duration::from_secs(5))
                                        .await
                                        .context("send ACK frame")?;
                                        break Err(err.into());
                                    }
                                }
                            }
                        }
                        OfferMessage::Deny { ty, detail: reject } => {
                            tracing::debug!(ty, "channel request rejected");
                            self.state = CallerHandshakeState::Ended;
                            break Err(reject.into());
                        }
                    }
                }
                CallerHandshakeState::Ended => {
                    self.state = CallerHandshakeState::Ended;
                    break Err(anyhow!("handshake already ended"));
                }
                CallerHandshakeState::Invalid => unreachable!(),
            }
        }
    }
}

enum ListenerHandshakeState<Selection> {
    WaitRequest,
    WaitAccept { selection: Selection },
    Ended,
    Invalid,
}
pub struct ListenerHandshake<Selector>
where
    Selector: ChannelSelector,
{
    state: ListenerHandshakeState<Selector::Selection>,
    selector: Selector,
}

impl<Selector> ListenerHandshake<Selector>
where
    Selector: ChannelSelector,
{
    #[allow(clippy::new_without_default)]
    pub fn new(selector: Selector) -> Self {
        Self {
            state: ListenerHandshakeState::WaitRequest,
            selector,
        }
    }

    pub async fn perform<S>(&mut self, sock: &mut S) -> Result<Selector::Selection>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        loop {
            match std::mem::replace(&mut self.state, ListenerHandshakeState::Invalid) {
                // wait for a channel request
                ListenerHandshakeState::WaitRequest => {
                    let req: RequestMessage = Frame::read_from(sock, Duration::from_secs(5))
                        .await
                        .context("read request frame")?
                        .unmarshal()?;
                    tracing::debug!(ty = req.ty, address = %req.address, "received channel request");
                    match self.selector.select(&req.ty, &req.address) {
                        Ok((offer, selection)) => {
                            tracing::debug!(req.ty, offer = ?offer, "send offer frame");
                            Frame::marshal(&OfferMessage::Offer { ty: req.ty, offer })?
                                .write_to(sock, Duration::from_secs(5))
                                .await
                                .context("send offer frame")?;
                            self.state = ListenerHandshakeState::WaitAccept { selection };
                        }
                        Err(rejection) => {
                            tracing::debug!(req.ty, rejection = ?rejection, "send rejection frame");
                            Frame::marshal(&OfferMessage::Deny {
                                ty: req.ty,
                                detail: rejection.clone(),
                            })?
                            .write_to(sock, Duration::from_secs(5))
                            .await
                            .context("send rejection frame)")?;
                            self.state = ListenerHandshakeState::Ended;
                            break Err(rejection.into());
                        }
                    }
                }
                // wait for the caller to accept the offer
                ListenerHandshakeState::WaitAccept { selection } => {
                    match Frame::read_from(sock, Duration::from_secs(5))
                        .await
                        .context("receive accept frame")?
                        .unmarshal::<AcceptMessage>()?
                    {
                        AcceptMessage::Accept { ty } => {
                            self.state = ListenerHandshakeState::Ended;
                            tracing::debug!(ty, "received ACK, handshake complete");
                            break Ok(selection);
                        }
                        AcceptMessage::Reject { ty, detail: reject } => {
                            self.state = ListenerHandshakeState::Ended;
                            tracing::debug!(ty, error = ?reject, "offer rejected");
                            break Err(reject.into());
                        }
                    }
                }
                // done
                ListenerHandshakeState::Ended => {
                    self.state = ListenerHandshakeState::Ended;
                    break Err(anyhow!("handshake already completed"));
                }
                ListenerHandshakeState::Invalid => unreachable!(),
            }
        }
    }
}

#[allow(unused)]
mod test {

    use tokio::net::UnixStream;

    use tokio::net::UnixListener;
    use tokio::task;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    use tracing::Instrument;

    use crate::testing::make_test_connection;

    use super::*;

    struct SimpleChannelSelector(String, ChannelAddress, Option<String>);

    struct SimpleAcceptor(Option<String>, ChannelAddress);

    impl ChannelSelector for SimpleChannelSelector {
        type Selection = ChannelAddress;

        fn select(
            &mut self,
            ty: &str,
            address: &ChannelAddress,
        ) -> Result<(ChannelOffer, Self::Selection), ChannelError> {
            if ty != self.0 {
                Err(ChannelError {
                    reason: "invalid type".to_string(),
                    code: 1,
                })
            } else if *address != self.1 {
                Err(ChannelError {
                    reason: "invalid address".to_string(),
                    code: 2,
                })
            } else {
                Ok((
                    ChannelOffer {
                        address: address.clone(),
                        info: self.2.clone(),
                    },
                    address.clone(),
                ))
            }
        }
    }

    impl ChannelAcceptor for SimpleAcceptor {
        fn accept(
            &mut self,
            ty: &str,
            requested_address: &ChannelAddress,
            offer: &ChannelOffer,
        ) -> Result<(), ChannelError> {
            if offer.info == self.0 && offer.address == self.1 {
                tracing::info!(ty, ?requested_address, ?offer, "rejecting offer");
                Err(ChannelError {
                    reason: "error".to_string(),
                    code: 1,
                })
            } else {
                tracing::info!(ty, ?requested_address, ?offer, "accepting offer");
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn handshake() {
        let (mut listener, mut caller) = make_test_connection();

        let join_a = task::spawn(
            async move {
                let mut handshake = ListenerHandshake::new(SimpleChannelSelector(
                    "http/api".to_string(),
                    ChannelAddress::Virtual("test1234".to_string()),
                    Some("some info".to_string()),
                ));
                handshake.perform(&mut listener).await.unwrap();
            }
            .instrument(tracing::error_span!("listener")),
        );

        let join_b = task::spawn(
            async move {
                let mut handshake = CallerHandshake::new(
                    "http/api".to_owned(),
                    ChannelAddress::Virtual("test1234".to_string()),
                    SimpleAcceptor(None, ChannelAddress::Virtual("test1234".to_string())),
                );
                handshake.perform(&mut caller).await.unwrap();
            }
            .instrument(tracing::error_span!("caller")),
        );

        join_a.await.unwrap();
        join_b.await.unwrap();
    }

    #[tokio::test]
    async fn deny() {
        let (mut listener, mut caller) = make_test_connection();

        let task_a = task::spawn(async move {
            let mut handshake = ListenerHandshake::new(SimpleChannelSelector(
                "http/api".to_string(),
                ChannelAddress::Virtual("valid".to_string()),
                None,
            ));
            match handshake.perform(&mut listener).await {
                Ok(_) => panic!("handshake should have failed"),
                Err(err) => {
                    assert!(matches!(err.downcast_ref::<ChannelError>(), Some(_)))
                }
            }
        });

        let task_b = task::spawn(async move {
            let mut handshake = CallerHandshake::new(
                "http/api".to_string(),
                ChannelAddress::Virtual("invalid".to_string()),
                SimpleAcceptor(None, ChannelAddress::Virtual("".to_string())),
            );
            match handshake.perform(&mut caller).await {
                Ok(_) => panic!("handshake should have failed"),
                Err(err) => {
                    assert!(matches!(err.downcast_ref::<ChannelError>(), Some(_)))
                }
            }
        });

        task_b.await;
        task_a.await;
    }

    #[tokio::test]
    async fn reject() {
        let (mut listener, mut caller) = make_test_connection();

        let task_a = task::spawn(async move {
            let mut handshake = ListenerHandshake::new(SimpleChannelSelector(
                "http/api".to_string(),
                ChannelAddress::Virtual("valid".to_string()),
                None,
            ));
            match handshake.perform(&mut listener).await {
                Ok(_) => panic!(),
                Err(err) => {
                    assert!(matches!(err.downcast_ref::<ChannelError>(), Some(_)))
                }
            }
        });

        let task_b = task::spawn(async move {
            let mut handshake = CallerHandshake::new(
                "http/api".to_string(),
                ChannelAddress::Virtual("valid".to_string()),
                SimpleAcceptor(None, ChannelAddress::Virtual("valid".to_string())),
            );
            match handshake.perform(&mut caller).await {
                Ok(_) => panic!(),
                Err(err) => {
                    assert!(matches!(err.downcast_ref::<ChannelError>(), Some(_)))
                }
            }
        });

        task_b.await;
        task_a.await;
    }
}
