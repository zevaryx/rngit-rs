//! Starting Reticulum and loading the client identity, the way the command
//! line programs do.
//!
//! `RNS_CONFIG` is the Reticulum config directory. `RNGIT_CONFIG` is the
//! directory holding `client_identity` (default `~/.rngit`), which is created
//! on first use. Both match stock rngit.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rns_identity::identity::Identity;
use rns_runtime::prelude::*;

use crate::error::Error;

/// A running Reticulum instance and the identity requests are made as.
pub struct Session {
    pub runtime: ReticulumHandle,
    pub identity: Identity,
}

/// Load `client_identity` from `dir`, making one if it isn't there.
pub fn load_or_create_identity(dir: &std::path::Path) -> Result<Identity, Error> {
    let path = dir.join("client_identity");
    if path.exists() {
        return Identity::from_file(&path).map_err(|e| Error::Transport(format!("could not read {}: {e}", path.display())));
    }
    std::fs::create_dir_all(dir).map_err(Error::git)?;
    let identity = Identity::new();
    identity.to_file(&path).map_err(|e| Error::Transport(format!("could not write {}: {e}", path.display())))?;
    Ok(identity)
}

impl Session {
    /// Start Reticulum, then wait `settle` for announces and paths to arrive.
    pub async fn start(settle: Duration) -> Result<Session, Error> {
        let dir = match std::env::var_os("RNGIT_CONFIG") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| Error::InvalidInput("HOME is not set".into()))?).join(".rngit"),
        };
        let identity = load_or_create_identity(&dir)?;
        let config = std::env::var("RNS_CONFIG").ok();
        let runtime = init(config.as_deref(), None, ShutdownSignal::new(), Arc::new(AtomicBool::new(true)))
            .await
            .map_err(|e| Error::Transport(format!("could not start Reticulum: {e}")))?;
        tokio::time::sleep(settle).await;
        Ok(Session { runtime, identity })
    }

    /// How long to wait for a path, from `RNGIT_PATH_WAIT` (seconds, default 5).
    pub fn settle_time() -> Duration {
        Duration::from_secs(std::env::var("RNGIT_PATH_WAIT").ok().and_then(|v| v.parse().ok()).unwrap_or(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_created_once_and_then_reloaded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("nested");
        let first = load_or_create_identity(&dir).unwrap();
        let again = load_or_create_identity(&dir).unwrap();
        assert_eq!(first.hash, again.hash);
        std::fs::write(dir.join("client_identity"), b"junk").unwrap();
        assert!(load_or_create_identity(&dir).is_err());
    }
}
