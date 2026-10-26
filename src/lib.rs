//! A client for rngit, Git over Reticulum.
//!
//! The protocol lives in [`Client`] and talks through the [`Transport`]
//! trait. With the `reticulum` feature (on by default), [`rns::RnsTransport`]
//! carries it over a live rsReticulum link.

pub mod bundle;
pub mod cli;
pub mod client;
#[cfg(feature = "reticulum")]
pub mod discovery;
pub mod error;
pub mod helper;
#[cfg(feature = "reticulum")]
pub mod rns;
#[cfg(feature = "reticulum")]
pub mod session;
pub mod sign;
pub mod transport;
pub mod url;
pub mod wire;

pub use client::{Client, Limits, RemoteRefs, Want};
pub use error::{Error, Result, Status};
pub use transport::{Response, Transport};
pub use url::RepoUrl;
