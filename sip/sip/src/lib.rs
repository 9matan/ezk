#![warn(unreachable_pub)]

use async_trait::async_trait;
use bytesstr::BytesStr;
use log::warn;
use sip_auth::{ClientAuthenticator, DigestAuthenticator, DigestCredentials, DigestError};
use sip_core::{Endpoint, IncomingRequest, Layer, MayTake};
use sip_types::{
    header::typed::{Contact, ContentType},
    uri::{sip::SipUri, NameAddr},
    Method, StatusCode,
};

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

#[derive(Default)]
struct ClientLayer {}

#[async_trait]
impl Layer for ClientLayer {
    fn name(&self) -> &'static str {
        "client"
    }

    async fn receive(&self, endpoint: &Endpoint, request: MayTake<'_, IncomingRequest>) {
        let invite = if request.line.method == Method::INVITE {
            request.take()
        } else {
            return;
        };

        let contact: SipUri = "sip:bob@example.com".parse().unwrap();
        let contact = Contact::new(NameAddr::uri(contact));

        let call = match IncomingCall::from_invite(endpoint.clone(), invite, contact) {
            Ok(call) => call,
            Err((mut invite, e)) => {
                log::warn!("Failed to create incoming call from INVITE, {e}");

                let response = endpoint.create_response(&invite, StatusCode::BAD_REQUEST, None);

                if let Err(e) = endpoint
                    .create_server_inv_tsx(&mut invite)
                    .respond_failure(response)
                    .await
                {
                    log::warn!("Failed to respond with BAD_REQUEST to incoming INVITE, {e}");
                }

                return;
            }
        };
    }
}

/// High level SIP client, must be constructed using [`ClientBuilder`]
///
/// Can be cheaply cloned.
#[derive(Clone)]
pub struct Client {
    endpoint: Endpoint,
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
