//! The seam between the protocol and whatever carries it.

use std::future::Future;

use crate::error::Error;

/// What came back for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// A normal reply: a status byte then the payload.
    Packet(Vec<u8>),
    /// A bulk reply sent as a Resource (a git bundle or a release artifact).
    /// It carries no status byte.
    File { data: Vec<u8>, metadata: Option<Vec<u8>> },
}

/// Sends one request over an identified link to the node's `git.repositories`
/// destination. `data` is the already MessagePack-encoded request body.
pub trait Transport: Send + Sync {
    fn request(&self, path: &str, data: Vec<u8>) -> impl Future<Output = Result<Response, Error>> + Send;
}
