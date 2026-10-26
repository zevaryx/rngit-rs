//! Finding rngit nodes by their announces.

use std::time::Duration;

use rns_runtime::prelude::*;
use tokio::time::Instant;

use crate::error::Error;

/// Longest node name we keep.
const MAX_NAME: usize = 128;

/// An rngit node heard on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAnnounce {
    /// The `git.repositories` destination to put in `rns://` URLs.
    pub destination: [u8; 16],
    pub hops: u8,
    /// The announce's app data, if any. Untrusted.
    pub app_data: Option<Vec<u8>>,
    /// `app_data` read as a display name, if it is one.
    pub name: Option<String>,
}

/// Read announce app data as a node name. Anything that isn't short, plain
/// UTF-8 text is not a name.
pub fn node_name(app_data: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(app_data).ok()?.trim();
    if text.is_empty() || text.chars().count() > MAX_NAME || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.to_string())
}

impl NodeAnnounce {
    pub fn new(destination: [u8; 16], hops: u8, app_data: Option<Vec<u8>>) -> Self {
        let name = app_data.as_deref().and_then(node_name);
        NodeAnnounce { destination, hops, app_data, name }
    }
}

/// Listen for `listen` and return each rngit node heard, nearest first.
/// A node announces when it starts and then every few hours, so a short
/// listen can come back empty; nodes already known to the runtime are not
/// replayed.
pub async fn discover(runtime: &ReticulumHandle, listen: Duration) -> Result<Vec<NodeAnnounce>, Error> {
    let mut subscription = runtime
        .subscribe_announces(Some("git.repositories".to_string()), false)
        .await
        .map_err(|e| Error::Transport(format!("could not listen for announces: {e}")))?;
    let deadline = Instant::now() + listen;
    let mut found: Vec<NodeAnnounce> = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, subscription.recv()).await {
        let node = NodeAnnounce::new(event.destination_hash, event.hops, event.app_data);
        match found.iter_mut().find(|n| n.destination == node.destination) {
            Some(known) => *known = node,
            None => found.push(node),
        }
    }
    let _ = subscription.close().await;
    found.sort_by_key(|n| n.hops);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_short_plain_text() {
        assert_eq!(node_name(b"  Lab Node "), Some("Lab Node".to_string()));
        assert_eq!(node_name("Nöde".as_bytes()), Some("Nöde".to_string()));
        assert_eq!(node_name(b""), None);
        assert_eq!(node_name(b"   "), None);
        assert_eq!(node_name(&[0xff, 0xfe]), None);
        assert_eq!(node_name(b"bad\x1b[31mname"), None);
        assert_eq!(node_name("x".repeat(129).as_bytes()), None);
        assert!(node_name("x".repeat(128).as_bytes()).is_some());
    }

    #[test]
    fn announce_keeps_raw_data_and_a_name_when_there_is_one() {
        let named = NodeAnnounce::new([1; 16], 2, Some(b"Home".to_vec()));
        assert_eq!(named.name.as_deref(), Some("Home"));
        let binary = NodeAnnounce::new([2; 16], 0, Some(vec![0x93, 0x01]));
        assert_eq!(binary.name, None);
        assert_eq!(binary.app_data, Some(vec![0x93, 0x01]));
        assert_eq!(NodeAnnounce::new([3; 16], 1, None).name, None);
    }
}
