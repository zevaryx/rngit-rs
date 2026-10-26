//! [`Transport`] over a live Reticulum link, using rsReticulum.

use std::time::Duration;

use rns_identity::identity::Identity;
use rns_runtime::prelude::*;
use tokio::sync::Mutex;

use crate::error::Error;
use crate::sign::Signer;
use crate::transport::{Response, Transport};

/// Talks to one rngit node: the `git.repositories` destination at `destination`.
///
/// The link is opened on first use, identified with `identity`, and reopened
/// when it has dropped. Only one request runs at a time.
pub struct RnsTransport {
    runtime: ReticulumHandle,
    identity: Identity,
    destination: [u8; 16],
    request_timeout: Duration,
    session: Mutex<Option<LinkSession>>,
}

impl RnsTransport {
    /// `identity` is the long-lived client identity the node authorizes.
    pub fn new(runtime: ReticulumHandle, identity: Identity, destination: [u8; 16]) -> Self {
        RnsTransport { runtime, identity, destination, request_timeout: Duration::from_secs(120), session: Mutex::new(None) }
    }

    /// How long one request may take, including a bulk transfer.
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    async fn connect(&self) -> Result<LinkSession, Error> {
        let options = LinkConnectOptions { identify: true, client_label: "rngit".to_string(), ..LinkConnectOptions::default() };
        self.runtime
            .connect_link(self.destination, self.identity.clone(), options)
            .await
            .map_err(|e| Error::Transport(format!("could not open a link: {e}")))
    }

    /// Close the link. The next request opens a new one.
    pub async fn disconnect(&self) {
        if let Some(session) = self.session.lock().await.take() {
            session.handle.close().await;
        }
    }
}

/// Sort a link response into a plain reply or a bulk file. rsReticulum hands a
/// plain reply back already unwrapped (status byte then payload); only a
/// Resource reply carries metadata, and that is how a file is told apart.
pub(crate) fn classify(response: LinkSessionResponse) -> Response {
    match response.metadata {
        None => Response::Packet(response.data),
        metadata => Response::File { data: response.data, metadata },
    }
}

impl Transport for RnsTransport {
    async fn request(&self, path: &str, data: Vec<u8>) -> Result<Response, Error> {
        let mut guard = self.session.lock().await;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.connect().await?);
            }
            let session = guard.as_ref().ok_or_else(|| Error::Transport("no link".into()))?;
            match session.handle.request(path, &data, Some(self.request_timeout)).await {
                Ok(response) => return Ok(classify(response)),
                // A dead link is worth one fresh try; anything else is final.
                Err(LinkSessionError::SessionClosed | LinkSessionError::LinkNotActive) if attempt == 0 => {
                    *guard = None;
                }
                Err(e) => return Err(Error::Transport(e.to_string())),
            }
        }
        Err(Error::Transport("link kept dropping".into()))
    }
}

/// Lets a Reticulum [`Identity`] sign releases and work documents.
pub struct IdentitySigner<'a>(pub &'a Identity);

impl Signer for IdentitySigner<'_> {
    fn identity_hash(&self) -> [u8; 16] {
        self.0.hash
    }

    fn public_key(&self) -> [u8; 64] {
        self.0.get_public_key()
    }

    fn sign(&self, message: &[u8]) -> Result<[u8; 64], Error> {
        self.0.sign(message).ok_or_else(|| Error::InvalidInput("identity has no private key".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(data: Vec<u8>, metadata: Option<Vec<u8>>) -> LinkSessionResponse {
        LinkSessionResponse { request_id: [0; 16], data, metadata, response_time: Duration::ZERO }
    }

    #[test]
    fn reply_without_metadata_is_a_packet() {
        assert_eq!(classify(response(b"\x00ok".to_vec(), None)), Response::Packet(b"\x00ok".to_vec()));
    }

    #[test]
    fn resource_reply_is_a_file() {
        let bundle = b"# v2 git bundle\n".to_vec();
        assert_eq!(
            classify(response(bundle.clone(), Some(vec![0x81, 0x01, 0x00]))),
            Response::File { data: bundle.clone(), metadata: Some(vec![0x81, 0x01, 0x00]) }
        );
    }

    #[test]
    fn identity_signer_signs_with_the_identity() {
        let identity = Identity::new();
        let signer = IdentitySigner(&identity);
        let sig = signer.sign(b"hello").unwrap();
        crate::sign::verify_signature(&signer.public_key(), b"hello", &sig).unwrap();
        assert_eq!(crate::sign::identity_hash(&signer.public_key()), signer.identity_hash());
    }
}
