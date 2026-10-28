//! Fetching a fork's or mirror's source from an `https://` repository, in
//! this process with gix. Nothing is run: stock rngit runs `git` for this and
//! also takes `ssh`, which gix can only do by running `ssh`, so it isn't taken.
//!
//! A sender picks the address, so a node asked to fetch is a way to reach
//! what only the node can: addresses on its own network are refused, redirects
//! aren't followed, a source can't carry a password (it would be kept in the
//! repository's config) and what is fetched is cut off at the size a repository
//! may come to and at the time a request may take.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// Whether a source is one this node fetches: `https://` to a public address
/// (`open` also allows `http://` and addresses on the node's own network, for
/// labs and tests).
pub(super) fn check(source: &str, open: bool) -> Result<(), String> {
    let url = gix::Url::from_bytes(source.into()).map_err(|_| "Invalid source URL".to_string())?;
    let scheme = match url.scheme {
        gix::url::Scheme::Https => "https",
        gix::url::Scheme::Http if open => "http",
        _ => return Err("Prohibited source URL: this node forks and mirrors from rns:// and https:// repositories only".into()),
    };
    if url.user().is_some() || url.password().is_some() {
        return Err("Prohibited source URL: it can't carry a login".into());
    }
    let host = url.host().filter(|h| !h.is_empty()).ok_or("Invalid source URL")?;
    if !open {
        let port = url.port.unwrap_or(if scheme == "https" { 443 } else { 80 });
        let found: Vec<_> =
            (host.trim_matches(['[', ']']), port).to_socket_addrs().map_err(|_| "The source's address isn't known")?.collect();
        if found.is_empty() || found.iter().any(|a| !public(a.ip())) {
            return Err("Prohibited source URL: it isn't a public address".into());
        }
    }
    Ok(())
}

/// Whether an address is one on the open internet.
fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_v4(v4),
            None => public_v6(v6),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (64..128).contains(&o[1]))
        || o[0] >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80)
}

/// Fetch every branch and tag of `source` into the bare repository at `dest`
/// and point HEAD where the source's does. Gives the names of the refs the
/// source has. Gives up after `timeout`, or once `dest` is bigger than `max_bytes`.
pub(super) fn pull(source: &str, dest: &Path, timeout: Duration, max_bytes: u64) -> Result<Vec<String>, String> {
    use gix::protocol::handshake::Ref;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut repo = gix::open(dest).map_err(|e| e.to_string())?;
    {
        let mut config = repo.config_snapshot_mut();
        config.set_raw_value_by("http", None, "followRedirects", "false").map_err(|e| e.to_string())?;
        config.set_raw_value_by("gitoxide", Some("http".into()), "connectTimeout", "30000").map_err(|e| e.to_string())?;
    }
    let remote = repo
        .remote_at(source)
        .and_then(|r| r.with_refspecs(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*", "HEAD"], gix::remote::Direction::Fetch))
        .map_err(|e| e.to_string())?;

    // Set once the fetch should stop: it is out of time or has grown too large.
    let stop = AtomicBool::new(false);
    let done = AtomicBool::new(false);
    let fetched = std::thread::scope(|scope| {
        scope.spawn(|| {
            let started = std::time::Instant::now();
            while !done.load(Ordering::Relaxed) {
                if started.elapsed() > timeout || super::dir_size(dest) > max_bytes {
                    stop.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let result = (|| {
            let connection =
                remote.connect(gix::remote::Direction::Fetch).map_err(|e| e.to_string())?.with_transport_options(options(&repo, source)?);
            let prepared = connection.prepare_fetch(gix::progress::Discard, Default::default()).map_err(|e| why(&e))?;
            let refs = prepared.ref_map().remote_refs.clone();
            prepared.receive(gix::progress::Discard, &stop).map_err(|e| why(&e))?;
            Ok::<_, String>(refs)
        })();
        done.store(true, Ordering::Relaxed);
        result
    });
    if stop.load(Ordering::Relaxed) {
        return Err("The fetch took too long or came to more than a repository may".into());
    }
    let refs = fetched?;

    let mut head = None;
    let mut kept = std::collections::HashSet::new();
    for r in &refs {
        match r {
            Ref::Symbolic { full_ref_name, target, .. } if full_ref_name == "HEAD" => head = Some(target.to_string()),
            Ref::Unborn { full_ref_name, target } if full_ref_name == "HEAD" => head = Some(target.to_string()),
            Ref::Direct { full_ref_name, .. } | Ref::Peeled { full_ref_name, .. } | Ref::Symbolic { full_ref_name, .. } => {
                kept.insert(full_ref_name.to_string());
            }
            Ref::Unborn { .. } => {}
        }
    }
    if let Some(head) = head.filter(|h| h.starts_with("refs/heads/") && !h.contains(['\n', '\0'])) {
        super::support::write_file(&dest.join("HEAD"), format!("ref: {head}\n").as_bytes())
            .map_err(|_| "Could not set HEAD".to_string())?;
    }
    Ok(kept.into_iter().collect())
}

/// gix's HTTP options for `source`, trusting the root certificates rngit comes
/// with as well as the system's: a small container image has no store of its
/// own, and without one no https source could be reached.
fn options(repo: &gix::Repository, source: &str) -> Result<Box<dyn std::any::Any>, String> {
    use gix::protocol::transport::client::blocking_io::http;
    use std::sync::{Arc, Mutex};

    let configured = repo.transport_options(source, None).map_err(|e| e.to_string())?;
    let mut options = configured.and_then(|o| o.downcast::<http::Options>().ok()).map(|o| *o).unwrap_or_default();
    let backend = http::reqwest::Options {
        configure_client: Some(Box::new(|builder| Ok(builder.tls_certs_merge(bundled_roots())))),
        configure_request: None,
    };
    options.backend = Some(Arc::new(Mutex::new(backend)));
    Ok(Box::new(options))
}

/// An error and the innermost reason for it: gix's own message is often only
/// "Transport handshake failed".
fn why(e: &dyn std::error::Error) -> String {
    let mut root = e;
    while let Some(next) = root.source() {
        root = next;
    }
    let (said, reason) = (e.to_string(), root.to_string());
    if said == reason { said } else { format!("{said}: {reason}") }
}

fn bundled_roots() -> Vec<reqwest::Certificate> {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().filter_map(|der| reqwest::Certificate::from_der(der).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundled_roots_are_trusted_whatever_the_system_has() {
        assert_eq!(bundled_roots().len(), webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init_bare(dir.path()).unwrap();
        let options = options(&repo, "https://example.com/a/b.git").unwrap();
        let options = options.downcast_ref::<gix::protocol::transport::client::blocking_io::http::Options>().unwrap();
        let backend = options.backend.as_ref().unwrap().lock().unwrap();
        let backend = backend.downcast_ref::<gix::protocol::transport::client::blocking_io::http::reqwest::Options>().unwrap();
        assert!(backend.configure_client.is_some());
    }

    #[test]
    fn a_failure_says_why() {
        let e = std::io::Error::other(std::io::Error::other("invalid peer certificate: UnknownIssuer"));
        assert_eq!(why(&e), "invalid peer certificate: UnknownIssuer");
        #[derive(Debug)]
        struct Handshake(std::io::Error);
        impl std::fmt::Display for Handshake {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("Transport handshake failed")
            }
        }
        impl std::error::Error for Handshake {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let e = Handshake(std::io::Error::other("No CA certificates were loaded from the system"));
        assert_eq!(why(&e), "Transport handshake failed: No CA certificates were loaded from the system");
    }

    #[test]
    fn only_public_addresses_count() {
        for ip in [
            "10.0.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "140.82.112.3", "2606:4700::1111"] {
            assert!(public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn sources_are_https_without_a_login_to_a_public_address() {
        for bad in [
            "http://example.com/a/b.git",
            "ssh://git@example.com/a/b.git",
            "git@example.com:a/b.git",
            "file:///tmp/x",
            "/tmp/x",
            "https://user:pw@example.com/a.git",
            "https://127.0.0.1/a.git",
            "https://[::1]/a.git",
            "https://169.254.169.254/latest",
            "https://",
        ] {
            assert!(check(bad, false).is_err(), "{bad}");
        }
        assert!(check("https://8.8.8.8/a/b.git", false).is_ok());
        assert!(check("http://127.0.0.1:8000/a.git", true).is_ok());
        assert!(check("ssh://git@example.com/a.git", true).is_err());
    }
}
