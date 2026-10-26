//! Error types shared by the whole crate.

use std::fmt;

/// The one-byte status that starts every rngit reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    NotAllowed,
    BadRequest,
    NotFound,
    ServerError,
    Unknown(u8),
}

impl Status {
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0x00 => Status::Ok,
            0x01 => Status::NotAllowed,
            0x02 => Status::BadRequest,
            0x03 => Status::NotFound,
            0xff => Status::ServerError,
            other => Status::Unknown(other),
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Status::Ok => 0x00,
            Status::NotAllowed => 0x01,
            Status::BadRequest => 0x02,
            Status::NotFound => 0x03,
            Status::ServerError => 0xff,
            Status::Unknown(other) => other,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Ok => f.write_str("ok"),
            Status::NotAllowed => f.write_str("not allowed"),
            Status::BadRequest => f.write_str("bad request"),
            Status::NotFound => f.write_str("not found"),
            Status::ServerError => f.write_str("server error"),
            Status::Unknown(byte) => write!(f, "unknown status 0x{byte:02x}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The link or request failed before the node answered.
    #[error("transport error: {0}")]
    Transport(String),
    /// The node answered with a non-OK status.
    #[error("node replied {status}: {message}")]
    Remote { status: Status, message: String },
    /// The node's reply did not look like anything rngit sends.
    #[error("malformed reply: {0}")]
    Protocol(String),
    /// A reply or input went over a size limit.
    #[error("{what} is {size} bytes, over the {limit} byte limit")]
    TooLarge { what: &'static str, size: usize, limit: usize },
    /// A value given by the caller can't be sent.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Local git work failed.
    #[error("git error: {0}")]
    Git(String),
    /// A push would not be a fast-forward and `force` was not set.
    #[error("push is not a fast-forward; fetch first or force it")]
    NonFastForward,
    /// A signature or hash check failed.
    #[error("verification failed: {0}")]
    Verify(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn git(err: impl fmt::Debug) -> Self {
        Error::Git(format!("{err:?}"))
    }
}
