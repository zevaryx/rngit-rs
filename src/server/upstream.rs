//! Forks and mirrors: repositories that follow another, `/git/fork`,
//! `/git/mirror` and `/git/sync`.
//!
//! The node makes the repository by fetching every branch and tag of the
//! source, and marks it in its own `config` as stock rngit does
//! (`repository.rngit.type` is `fork` or `mirror`, `repository.rngit.upstream.source`
//! the source, `repository.rngit.upstream.sync` when it last followed it), so
//! either node can serve the folder. A sync fetches again, taking every
//! branch and tag as the source has it. A mirror's HEAD follows the source's;
//! a fork's is left to whoever keeps it (the pull sets it only when the repository is made).
//!
//! Sources are `rns://` repositories, fetched by the node's [`Upstream`] (see
//! [`crate::serve::upstream`]) with the node's own identity, and `https://`
//! ones, fetched in this process (see `web`). Stock also fetches `ssh` sources
//! by running `git`; this node runs nothing. A sender picks the source, so
//! who may fork and mirror (`c` on a group) is who the node trusts to have it
//! read what only the node may read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rmpv::Value;

use super::support::{now, refuse, write_file};
use super::{Node, Perm, Refusal, ack, dir_size, int_key, split_repo_path};
use crate::error::Status;
use crate::transport::Response;
use crate::url::RepoUrl;
use crate::wire::MapRef;

/// What fetches from other nodes for this one.
pub trait Upstream: Send + Sync + std::fmt::Debug {
    /// Make `dest`, a bare repository (made if there is none), hold every
    /// branch and tag of `source` and point its HEAD where the source's does.
    /// Give up after `timeout`.
    fn pull(&self, source: &RepoUrl, dest: &Path, timeout: Duration) -> Result<(), String>;

    /// The names of the branches and tags `source` has, for a node that drops
    /// what a source dropped ([`Node::prune_synced`]). Without it, that node's
    /// syncs from `source` fail rather than keep what they should drop.
    fn refs(&self, _source: &RepoUrl, _timeout: Duration) -> Result<Vec<String>, String> {
        Err("This node can't list what a source has".into())
    }
}

/// What a repository follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Fork,
    Mirror,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Fork => "fork",
            Kind::Mirror => "mirror",
        }
    }
}

/// A repository that follows another: how, what and when it last did.
struct Following {
    kind: Kind,
    source: String,
    synced: u64,
}

fn config_path(repo: &Path) -> PathBuf {
    repo.join("config")
}

fn read_following(repo: &Path) -> Option<Following> {
    let config = gix::config::File::from_path_no_includes(config_path(repo), gix::config::Source::Local).ok()?;
    let kind = match config.string_by("repository", Some("rngit".into()), "type")?.to_string().as_str() {
        "fork" => Kind::Fork,
        "mirror" => Kind::Mirror,
        _ => return None,
    };
    let source = config.string_by("repository", Some("rngit.upstream".into()), "source")?.to_string();
    let synced =
        config.string_by("repository", Some("rngit.upstream".into()), "sync").and_then(|t| t.to_string().parse().ok()).unwrap_or(0);
    Some(Following { kind, source, synced })
}

/// Write what a repository follows, and when it did.
fn write_following(repo: &Path, kind: Option<Kind>, source: &str, synced: u64) -> Result<(), ()> {
    let mut config = gix::config::File::from_path_no_includes(config_path(repo), gix::config::Source::Local).map_err(drop)?;
    let mut set = |subsection: &str, key: &str, value: &str| {
        config.set_raw_value_by("repository", Some(subsection.into()), key, value).map(drop).map_err(drop)
    };
    if let Some(kind) = kind {
        set("rngit", "type", kind.word())?;
        set("rngit.upstream", "source", source)?;
    }
    set("rngit.upstream", "sync", &synced.to_string())?;
    write_file(&config_path(repo), &config.to_bstring()).map_err(drop)
}

/// Where a fork or mirror gets its commits.
enum Source {
    /// A repository on an rngit node, fetched by the node's [`Upstream`].
    Rns(RepoUrl),
    /// An `https://` repository, fetched in this process.
    Web(String),
}

impl Source {
    fn parse(written: &str) -> Option<Source> {
        match written.parse::<RepoUrl>() {
            Ok(url) => Some(Source::Rns(url)),
            Err(_) => written.to_ascii_lowercase().starts_with("http").then(|| Source::Web(written.to_string())),
        }
    }
}

/// The source a request names, as it was written and as an address.
fn named_source(map: &MapRef, open: bool) -> Result<(String, Source), Refusal> {
    let source = match map.get("source") {
        None => return Err(refuse(Status::BadRequest, "No source specified")),
        Some(Value::String(s)) => s.as_str().unwrap_or_default().to_string(),
        Some(_) => return Err(refuse(Status::BadRequest, "Invalid source URL")),
    };
    if source.is_empty() {
        return Err(refuse(Status::BadRequest, "No source specified"));
    }
    let lower = source.to_ascii_lowercase();
    if lower.starts_with("rns://") {
        let url = source.parse().map_err(|_| refuse(Status::BadRequest, "Invalid source URL"))?;
        return Ok((source, Source::Rns(url)));
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        #[cfg(feature = "https")]
        {
            super::web::check(&source, open).map_err(|e| refuse(Status::NotAllowed, &e))?;
            return Ok((source.clone(), Source::Web(source)));
        }
    }
    let _ = open;
    Err(refuse(Status::NotAllowed, "Prohibited source URL: this node forks and mirrors from rns:// and https:// repositories only"))
}

/// The branches and tags a repository has.
fn local_refs(repo: &Path) -> Vec<String> {
    let Ok(repo) = gix::open(repo) else { return Vec::new() };
    let Ok(platform) = repo.references() else { return Vec::new() };
    let (Ok(heads), Ok(tags)) = (platform.local_branches(), platform.tags()) else { return Vec::new() };
    heads.chain(tags).flatten().map(|r| r.name().as_bstr().to_string()).collect()
}

impl Node {
    fn upstream_or_refuse(&self) -> Result<&dyn Upstream, Refusal> {
        self.upstream.as_deref().ok_or(refuse(Status::NotAllowed, "This node can't fetch from other nodes"))
    }

    /// Fetch from `source` into the repository at `dir`, and check it hasn't
    /// grown past what a repository may. With `want_refs`, gives the names of the branches and tags the source has.
    fn pull_into(&self, source: &Source, dir: &Path, want_refs: bool) -> Result<Option<Vec<String>>, String> {
        let refs = match source {
            Source::Rns(url) => {
                let upstream = self.upstream.as_deref().ok_or("This node can't fetch from other nodes")?;
                // Listed first, so a ref made while fetching isn't taken for one the source dropped.
                let listed = if want_refs { Some(upstream.refs(url, self.max_request_time)?) } else { None };
                upstream.pull(url, dir, self.max_request_time)?;
                listed
            }
            #[cfg(feature = "https")]
            Source::Web(url) => {
                super::web::check(url, self.open_sources)?;
                let listed = super::web::pull(url, dir, self.max_request_time, self.quotas.max_repo_bytes)?;
                want_refs.then_some(listed)
            }
            #[cfg(not(feature = "https"))]
            Source::Web(url) => return Err(format!("This node was built without https:// sources, so can't fetch {url}")),
        };
        if dir_size(dir) > self.quotas.max_repo_bytes {
            return Err("The repository is larger than this node keeps".into());
        }
        Ok(refs)
    }

    /// `/git/fork` and `/git/mirror`: make a repository from a source, whose
    /// maker is its administrator.
    pub(super) fn fork_or_mirror(&self, request: &Value, map: &MapRef, peer: &[u8; 16], kind: Kind) -> Result<Response, Refusal> {
        if *peer == super::NULL_IDENT {
            return Err(refuse(Status::NotAllowed, "Not identified"));
        }
        let (written, source) = named_source(map, self.open_sources)?;
        let name = int_key(request, 0).ok_or(refuse(Status::BadRequest, "No repository specified"))?;
        let (group, repo) = split_repo_path(&name).ok_or(refuse(Status::BadRequest, "Invalid request"))?;
        let group_dir = self.root.join(group);
        if !group_dir.is_dir() {
            return Err(refuse(Status::NotFound, "Not found"));
        }
        if !self.allowed_in_group(group, peer, Perm::Create) {
            return Err(if self.allowed_in_group(group, peer, Perm::Read) {
                refuse(Status::NotAllowed, "Not allowed")
            } else {
                refuse(Status::NotFound, "Not found")
            });
        }
        let final_dir = group_dir.join(repo);
        if final_dir.exists() {
            return Err(if self.allowed(group, repo, peer, Perm::Read) {
                refuse(Status::NotAllowed, "Repository already exists")
            } else {
                refuse(Status::NotFound, "Not found")
            });
        }
        let kept = std::fs::read_dir(&group_dir).map_or(0, |entries| entries.flatten().filter(|e| e.path().is_dir()).count());
        if kept >= self.quotas.max_repos_per_group {
            return Err(refuse(Status::ServerError, "Too many repositories in this group"));
        }
        if matches!(source, Source::Rns(_)) {
            self.upstream_or_refuse()?;
        }

        // Made beside where it will be, under a name nothing serves, and moved into place once whole.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let tmp = group_dir.join(format!(".{}-{nanos}.part", kind.word()));
        let made = (|| {
            gix::init_bare(&tmp).map_err(|_| refuse(Status::ServerError, "Failed to initialize repository"))?;
            self.pull_into(&source, &tmp, false).map_err(|e| refuse(Status::ServerError, &format!("Failed to fetch from source: {e}")))?;
            write_following(&tmp, Some(kind), &written, now())
                .map_err(|()| refuse(Status::ServerError, "Failed to configure repository type"))?;
            let allowed = group_dir.join(format!("{repo}.allowed"));
            write_file(&allowed, format!("adm:{}", hex::encode(peer)).as_bytes())
                .map_err(|_| refuse(Status::ServerError, "Could not initialize repository"))?;
            if final_dir.exists() || std::fs::rename(&tmp, &final_dir).is_err() {
                let _ = std::fs::remove_file(&allowed);
                return Err(refuse(Status::ServerError, "Could not write repository"));
            }
            Ok(())
        })();
        if made.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        made.map(|()| ack())
    }

    /// `/git/sync`: follow the source of a fork or mirror again.
    pub(super) fn sync(&self, request: &Value, peer: &[u8; 16]) -> Result<Response, Refusal> {
        if *peer == super::NULL_IDENT {
            return Err(refuse(Status::NotAllowed, "Not identified"));
        }
        let name = int_key(request, 0).ok_or(refuse(Status::BadRequest, "No repository specified"))?;
        let (group, repo) = split_repo_path(&name).ok_or(refuse(Status::NotFound, "Not found"))?;
        let dir = self.root.join(group).join(repo);
        if !dir.is_dir() || !self.allowed(group, repo, peer, Perm::Read) {
            return Err(refuse(Status::NotFound, "Not found"));
        }
        if !self.allowed(group, repo, peer, Perm::Write) {
            return Err(refuse(Status::NotAllowed, "Not allowed"));
        }
        let following = read_following(&dir).ok_or(refuse(Status::BadRequest, "Repository is neither fork nor mirror"))?;
        if matches!(Source::parse(&following.source), Some(Source::Rns(_))) {
            self.upstream_or_refuse()?;
        }
        self.follow(&dir, &following)
            .map(|()| ack())
            .map_err(|_| refuse(Status::ServerError, if following.kind == Kind::Fork { "Fork sync failed" } else { "Mirror sync failed" }))
    }

    fn follow(&self, dir: &Path, following: &Following) -> Result<(), String> {
        let source = Source::parse(&following.source).ok_or("The source isn't an rns:// or https:// repository")?;
        // A fork's HEAD is its keeper's to move; the pull sets it from the source's.
        let head = (following.kind == Kind::Fork).then(|| std::fs::read(dir.join("HEAD")).ok()).flatten();
        let before = if self.prune_synced { local_refs(dir) } else { Vec::new() };
        let refs = self.pull_into(&source, dir, self.prune_synced)?;
        if let Some(refs) = refs {
            // Only what was here before the pull and the source no longer has.
            let repo = gix::open(dir).map_err(|_| "Could not open the repository to drop what the source dropped".to_string())?;
            for name in before.into_iter().filter(|n| !refs.contains(n)) {
                if let Ok(reference) = repo.find_reference(name.as_str()) {
                    let _ = reference.delete();
                }
            }
        }
        if let Some(head) = head {
            write_file(&dir.join("HEAD"), &head).map_err(|_| "Could not keep the fork's HEAD".to_string())?;
        }
        write_following(dir, None, &following.source, now()).map_err(|()| "Could not record the sync".to_string())
    }

    /// Bring every mirror up to date that hasn't followed its source for
    /// `older_than`. How many were.
    pub fn sync_mirrors(&self, older_than: Duration) -> usize {
        let mut synced = 0;
        let Ok(groups) = std::fs::read_dir(&self.root) else { return 0 };
        for group in groups.flatten().filter(|g| g.path().is_dir()) {
            let Ok(repos) = std::fs::read_dir(group.path()) else { continue };
            for repo in repos.flatten() {
                let name = repo.file_name().to_string_lossy().into_owned();
                if split_repo_path(&format!("{}/{name}", group.file_name().to_string_lossy())).is_none() {
                    continue;
                }
                let Some(following) = read_following(&repo.path()).filter(|f| f.kind == Kind::Mirror) else { continue };
                if now().saturating_sub(following.synced) >= older_than.as_secs() && self.follow(&repo.path(), &following).is_ok() {
                    synced += 1;
                }
            }
        }
        synced
    }
}
