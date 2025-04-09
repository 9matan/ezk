#![warn(unreachable_pub)]


use async_trait::async_trait;
use bytesstr::BytesStr;
use incoming_call::NoMedia;
use log::warn;
use tokio::sync::Mutex;
use sip_auth::{ClientAuthenticator, DigestAuthenticator, DigestCredentials, DigestError};
use sip_core::{Endpoint, IncomingRequest, Layer, MayTake};
use sip_types::{
    header::typed::{Contact, ContentType, FromTo},
    uri::{sip::SipUri, NameAddr},
    Method, StatusCode,
};
use std::{sync::Arc, collections::VecDeque};

mod call;
mod client_builder;
mod incoming_call;
mod media;
mod outbound_call;
mod registration;

pub use call::{Call, CallEvent};
pub use client_builder::ClientBuilder;
pub use incoming_call::{IncomingCall, IncomingCallFromInviteError};
pub use media::{
    Codec, MediaBackend, MediaEvent, MediaSession, RtpReceiver, RtpSendError, RtpSender,
};
pub use outbound_call::{MakeCallError, OutboundCall, UnacknowledgedCall};
pub use registration::{RegisterError, RegistrarConfig, Registration};

const CONTENT_TYPE_SDP: ContentType = ContentType(BytesStr::from_static("application/sdp"));

slotmap::new_key_type! {
    struct AccountId;
}

struct ClientLayer {
    invites_queue: Arc<Mutex<VecDeque<IncomingRequest>>>,
}

impl ClientLayer {
    pub(crate) fn new(invites_queue: Arc<Mutex<VecDeque<IncomingRequest>>>) -> Self {
        Self { invites_queue }
    }
}

#[async_trait]
impl Layer for ClientLayer {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn receive(&self, _endpoint: &Endpoint, request: MayTake<'_, IncomingRequest>) {
        let invite = if request.line.method == Method::INVITE {
            request.take()
        } else {
            return;
        };

        let mut queue = self.invites_queue.lock().await;
        queue.push_back(invite);
    }
}

/// High level SIP client, must be constructed using [`ClientBuilder`]
///
/// Can be cheaply cloned.
#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
    invites_queue: Arc<Mutex<VecDeque<IncomingRequest>>>,
}

impl Client {
    /// Create a [`ClientBuilder`]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub async fn register<A: ClientAuthenticator + Send + 'static>(
        &self,
        config: RegistrarConfig,
        authenticator: A,
    ) -> Result<Registration, RegisterError<A::Error>> {
        Registration::register(self.endpoint.clone(), config, authenticator).await
    }

    pub async fn make_call<M: MediaBackend>(
        &self,
        id: NameAddr,
        contact: Contact,
        target: SipUri,
        media: M,
    ) -> Result<OutboundCall<M>, MakeCallError<M::Error, DigestError>> {
        OutboundCall::make(
            self.endpoint.clone(),
            DigestAuthenticator::new(DigestCredentials::new()),
            id,
            contact,
            target,
            media,
        )
        .await
    }

    pub async fn get_incoming_call(&self, contact: Contact) -> Result<Option<(IncomingCall<NoMedia>, FromTo)>, (IncomingRequest, IncomingCallFromInviteError)> {
        let invite = self.invites_queue.lock().await.pop_front();
        match invite {
            Some(invite) => {
                let from = invite.base_headers.from.clone();
                IncomingCall::from_invite(self.endpoint.clone(), invite, contact)
                    .map(|incoming_call| Some((incoming_call, from)))
            },
            None => Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{call::CallEvent, Client, MediaSession, RegistrarConfig};
    use rtc_proto::{BundlePolicy, Codec, Codecs, Options, RtcpMuxPolicy, TransportType};
    use sip_auth::{DigestAuthenticator, DigestCredentials, DigestUser};
    use std::time::Duration;

    #[tokio::test]
    async fn test() {
        env_logger::init();
        let client = Client::builder()
            .listen_udp("10.6.0.3:5060".parse().unwrap())
            .build()
            .await
            .unwrap();

        let mut credentials = DigestCredentials::new();
        credentials.add_for_realm("ccmsipline", DigestUser::new("2001", "opentalk"));

        let reg = client
            .register(
                RegistrarConfig {
                    registrar: "sip:10.6.6.6:5060".parse().unwrap(),
                    username: "2001".into(),
                    override_id: None,
                    override_contact: None,
                },
                DigestAuthenticator::new(credentials.clone()),
            )
            .await
            .unwrap();

        let mut sdp_session = rtc::AsyncSdpSession::new(
            "10.6.0.3".parse().unwrap(),
            Options {
                offer_transport: TransportType::Rtp,
                offer_ice: false,
                offer_avpf: false,
                rtcp_mux_policy: RtcpMuxPolicy::Negotiate,
                bundle_policy: BundlePolicy::MaxCompat,
            },
        );

        let audio = sdp_session
            .add_local_media(
                Codecs::new(rtc_proto::MediaType::Audio).with_codec(Codec::G722),
                1,
                rtc_proto::Direction::SendRecv,
            )
            .unwrap();

        let video = sdp_session
            .add_local_media(
                Codecs::new(rtc_proto::MediaType::Video).with_codec(Codec::H264.with_pt(97)),
                1,
                rtc_proto::Direction::SendRecv,
            )
            .unwrap();

        sdp_session.add_media(audio, rtc_proto::Direction::SendRecv);
        sdp_session.add_media(video, rtc_proto::Direction::SendRecv);

        let mut outbound = reg
            .make_call(
                "sip:2002@10.6.6.6".parse().unwrap(),
                DigestAuthenticator::new(credentials),
                MediaSession::new(sdp_session),
            )
            .await
            .unwrap();

        let completed = match tokio::time::timeout(
            Duration::from_secs(5),
            outbound.wait_for_completion(),
        )
        .await
        {
            Ok(completed) => completed.unwrap(),
            Err(_) => {
                outbound.cancel().await.unwrap();
                return;
            }
        };

        let mut call = completed.finish().await.unwrap();

        #[allow(clippy::while_let_loop)]
        loop {
            match call.run().await.unwrap() {
                CallEvent::Media(event) => match event {
                    crate::MediaEvent::SenderAdded { sender, codec } => {}
                    crate::MediaEvent::ReceiverAdded {
                        mut receiver,
                        codec,
                    } => {
                        tokio::spawn(async move {
                            while let Some(packet) = receiver.recv().await {
                                println!("Got rtp");
                            }
                        });
                    }
                },
                CallEvent::Terminated => break,
            }
        }
    }
}
