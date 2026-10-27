//! Serving repositories to rngit clients.
//!
//! [`Node`] answers the requests of the protocol from repositories in a
//! folder, with no network in it: the caller hands over a request path, its
//! body and who sent it, and sends back what [`Node::handle`] returns. It
//! answers `/git/list`, `/git/fetch`, `/git/push`, `/git/create`,
//! `/git/delete`, `/git/fork`, `/git/mirror`, `/git/sync`, `/mgmt/release`,
//! `/mgmt/perms` and `/mgmt/work`.
//!
//! The folder holds one folder per group and `name.git` repositories in each.
//! Who may do what comes from `.allowed` files, see [`Rule`].

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gix::ObjectId;
use rmpv::Value;

use crate::bundle;
use crate::error::Status;
use crate::transport::Response;
use crate::wire::{self, MapRef};

pub use upstream::Upstream;

mod access;
mod perms;
mod releases;
mod support;
mod upstream;
mod work;

/// The hash stock rngit gives a peer that didn't identify itself.
pub const NULL_IDENT: [u8; 16] = [0xd7, 0xdb, 0x22, 0xf6, 0x3b, 0x45, 0x3c, 0x23, 0xbb, 0x06, 0x88, 0xdd, 0xe5, 0x65, 0xb7, 0xc1];

/// Most refs listed or asked for in one request.
const MAX_REFS: usize = 10_000;
/// Most hashes a client may say it already holds.
const MAX_HAVES: usize = 1024;
/// Most refs one bundle-less push may move.
const MAX_OPERATIONS: usize = 1000;
/// Largest request body read.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Largest `.allowed` file read.
const MAX_RULES_BYTES: u64 = 64 * 1024;

/// What an identity may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Perm {
    Read,
    Write,
    Create,
    Stats,
    Release,
    Issues,
    Proposals,
    Admin,
}

impl Perm {
    fn from_word(word: &str) -> Option<Vec<Perm>> {
        Some(match word {
            "r" | "read" => vec![Perm::Read],
            "w" | "write" => vec![Perm::Write],
            "rw" | "readwrite" => vec![Perm::Read, Perm::Write],
            "c" | "create" => vec![Perm::Create],
            "s" | "stats" => vec![Perm::Stats],
            "rel" | "release" => vec![Perm::Release],
            "i" | "interact" => vec![Perm::Issues],
            "p" | "propose" => vec![Perm::Proposals],
            "adm" | "admin" => vec![Perm::Admin],
            _ => return None,
        })
    }
}

/// Who a rule is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Everyone,
    Nobody,
    Identity([u8; 16]),
}

/// One line of a `.allowed` file or of the node's settings: `r:all`,
/// `rw:0123456789abcdef0123456789abcdef`, `adm:...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub perms: Vec<Perm>,
    pub who: Who,
}

impl Rule {
    /// Read rules separated by new lines or commas. A line that isn't a rule
    /// is skipped, so a typo can only take permissions away.
    pub fn parse(text: &str) -> Vec<Rule> {
        let mut rules = Vec::new();
        for item in text.split(['\n', ',']) {
            let item = item.trim();
            let Some((words, who)) = item.split_once(':') else { continue };
            let who = match who.trim().to_ascii_lowercase().as_str() {
                "all" | "a" | "everyone" => Who::Everyone,
                "none" | "n" | "nobody" => Who::Nobody,
                hash => match decode_identity(hash) {
                    Some(id) => Who::Identity(id),
                    None => continue,
                },
            };
            let perms: Option<Vec<Perm>> =
                words.split_whitespace().map(|w| Perm::from_word(&w.to_ascii_lowercase())).collect::<Option<Vec<_>>>().map(|p| p.concat());
            if let Some(perms) = perms.filter(|p| !p.is_empty()) {
                rules.push(Rule { perms, who });
            }
        }
        rules
    }
}

fn decode_identity(hash: &str) -> Option<[u8; 16]> {
    hex::decode(hash).ok()?.try_into().ok()
}

/// A folder of repositories, and the rules for who may use them.
#[derive(Debug, Clone, Default)]
pub struct Node {
    /// Holds one folder per group.
    pub root: PathBuf,
    /// Rules per group from the node's own settings, applied after the
    /// `.allowed` files.
    pub group_rules: HashMap<String, Vec<Rule>>,
    /// Rules for every group, applied after the others.
    pub default_rules: Vec<Rule>,
    /// Peers turned away whatever the rules say.
    pub blocked: HashSet<[u8; 16]>,
    /// Largest bundle sent in one reply.
    pub max_bundle_bytes: usize,
    /// How long building or unpacking one bundle may take.
    pub max_request_time: Duration,
    /// What a peer who may write can add.
    pub quotas: Quotas,
    /// Bundles built lately, so a second clone of the same thing costs nothing.
    cache: Arc<Mutex<BundleCache>>,
    /// Held while a work document is written, so two never get one number.
    working: Arc<Mutex<()>>,
    /// What fetches from other nodes, for forks, mirrors and syncs. Without
    /// one they are refused.
    pub upstream: Option<Arc<dyn Upstream>>,
}

/// Who sent a request: the identity hash they proved on their link, and the
/// public key of that identity if whatever carries the request knows it
/// (what a work document is signed with is checked against it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sender {
    pub hash: [u8; 16],
    pub key: Option<[u8; 64]>,
}

/// What writing to the node may add, so one peer can't fill the disk.
#[derive(Debug, Clone, Copy)]
pub struct Quotas {
    /// Largest bundle taken in a push.
    pub max_push_bytes: usize,
    /// Largest file added to a release.
    pub max_artifact_bytes: usize,
    /// What one release's files may come to.
    pub max_release_bytes: u64,
    /// What one repository may come to on disk, counting a push.
    pub max_repo_bytes: u64,
    /// Repositories in one group.
    pub max_repos_per_group: usize,
    pub pack: bundle::PackLimits,
}

impl Default for Quotas {
    fn default() -> Self {
        Quotas {
            max_push_bytes: 256 * 1024 * 1024,
            max_artifact_bytes: 64 * 1024 * 1024,
            max_release_bytes: 256 * 1024 * 1024,
            max_repo_bytes: 1024 * 1024 * 1024,
            max_repos_per_group: 1000,
            pack: bundle::PackLimits {
                max_objects: 2_000_000,
                max_object_bytes: 128 * 1024 * 1024,
                max_total_bytes: 2 * 1024 * 1024 * 1024,
            },
        }
    }
}

/// How long a bundle is kept, and how much of them in all.
const CACHE_TTL: Duration = Duration::from_secs(600);
const CACHE_BYTES: usize = 64 * 1024 * 1024;

/// What a bundle was built from: the repository, and for each ref wanted its
/// object, what the receiver holds of it, and the other tips it holds.
type CacheKey = (PathBuf, Vec<(String, ObjectId, Option<ObjectId>)>, Vec<ObjectId>);

#[derive(Debug, Default)]
struct BundleCache {
    entries: Vec<(CacheKey, Instant, Arc<Vec<u8>>)>,
}

impl BundleCache {
    fn get(&mut self, key: &CacheKey) -> Option<Arc<Vec<u8>>> {
        self.entries.retain(|(_, at, _)| at.elapsed() < CACHE_TTL);
        self.entries.iter().find(|(k, ..)| k == key).map(|(_, _, data)| data.clone())
    }

    /// Keep `data`, dropping the oldest bundles to stay within [`CACHE_BYTES`].
    /// A bundle too big for that on its own isn't kept.
    fn put(&mut self, key: CacheKey, data: Arc<Vec<u8>>) {
        if data.len() > CACHE_BYTES / 2 {
            return;
        }
        self.entries.push((key, Instant::now(), data));
        while self.entries.iter().map(|(_, _, d)| d.len()).sum::<usize>() > CACHE_BYTES {
            self.entries.remove(0);
        }
    }
}

/// A request that can't be answered as asked.
struct Refusal(Status, String);

impl Refusal {
    fn new(status: Status, text: impl Into<String>) -> Self {
        Refusal(status, text.into())
    }

    fn reply(&self) -> Response {
        let mut out = vec![self.0.to_byte()];
        out.extend_from_slice(self.1.as_bytes());
        Response::Packet(out)
    }
}

impl Node {
    /// How many built bundles are being kept.
    pub fn cached_bundles(&self) -> usize {
        self.cache.lock().map_or(0, |cache| cache.entries.len())
    }

    pub fn new(root: PathBuf) -> Self {
        Node { root, max_bundle_bytes: 256 * 1024 * 1024, max_request_time: Duration::from_secs(120), ..Node::default() }
    }

    /// Answer one request. `peer` is the identity hash the sender proved on
    /// its link, if it did. `None` means the path isn't one this node serves.
    pub fn handle(&self, path: &str, data: &[u8], peer: Option<[u8; 16]>) -> Option<Response> {
        self.handle_from(path, data, peer.map(|hash| Sender { hash, key: None }))
    }

    /// Like [`Node::handle`], for a sender whose public key is known: work
    /// documents can't be written without it, as they carry signatures.
    pub fn handle_from(&self, path: &str, data: &[u8], sender: Option<Sender>) -> Option<Response> {
        let known = matches!(
            path,
            "/git/list"
                | "/git/fetch"
                | "/git/push"
                | "/git/create"
                | "/git/fork"
                | "/git/sync"
                | "/git/mirror"
                | "/git/delete"
                | "/mgmt/perms"
                | "/mgmt/release"
                | "/mgmt/work"
        );
        if !known {
            return None;
        }
        let peer = sender.map_or(NULL_IDENT, |s| s.hash);
        let key = sender.and_then(|s| s.key);
        let result = (|| {
            if self.blocked.contains(&peer) {
                return Err(Refusal::new(Status::NotAllowed, "Not allowed"));
            }
            // A push carries its bundle.
            let most = match path {
                "/git/push" => MAX_REQUEST_BYTES + self.quotas.max_push_bytes,
                // A release's file comes in one request.
                "/mgmt/release" => MAX_REQUEST_BYTES + self.quotas.max_artifact_bytes,
                _ => MAX_REQUEST_BYTES,
            };
            if data.len() > most {
                return Err(Refusal::new(Status::BadRequest, "Request is too large"));
            }
            let request = wire::decode(data).map_err(|_| Refusal::new(Status::BadRequest, "Invalid request"))?;
            let map = MapRef::new(&request).map_err(|_| Refusal::new(Status::BadRequest, "Invalid request"))?;
            match path {
                "/git/list" => self.list(&request, &map, &peer),
                "/git/fetch" => self.fetch(&request, &map, &peer),
                "/git/push" => self.push(&request, &map, &peer),
                "/git/create" => self.create(&request, &peer),
                "/git/delete" => self.delete_ref(&request, &map, &peer),
                "/mgmt/release" => self.release(&request, &map, &peer),
                "/git/fork" => self.fork_or_mirror(&request, &map, &peer, upstream::Kind::Fork),
                "/git/mirror" => self.fork_or_mirror(&request, &map, &peer, upstream::Kind::Mirror),
                "/git/sync" => self.sync(&request, &peer),
                "/mgmt/perms" => self.perms(&request, &map, &peer),
                "/mgmt/work" => self.work(&request, &map, &peer, key.as_ref()),
                _ => Err(Refusal::new(Status::NotAllowed, "Not allowed")),
            }
        })();
        Some(result.unwrap_or_else(|refusal| refusal.reply()))
    }

    /// The repository a request names, if the peer may read it. A repository
    /// the peer may not read looks the same as one that isn't there.
    fn readable(&self, request: &Value, peer: &[u8; 16]) -> Result<(String, gix::Repository), Refusal> {
        let name = int_key(request, 0).ok_or(Refusal::new(Status::BadRequest, "No repository specified"))?;
        let (group, repo) = split_repo_path(&name).ok_or(Refusal::new(Status::NotFound, "Not found"))?;
        let dir = self.root.join(group).join(repo);
        if !dir.is_dir() || !self.allowed(group, repo, peer, Perm::Read) {
            return Err(Refusal::new(Status::NotFound, "Not found"));
        }
        let opened = gix::open(&dir).map_err(|_| Refusal::new(Status::ServerError, "Repository is unreadable"))?;
        Ok((name, opened))
    }

    /// The repository a request names, to be changed. A peer who can't read it
    /// is told it isn't there; one who can read but not write, that it isn't allowed.
    fn writable(&self, request: &Value, peer: &[u8; 16]) -> Result<(String, gix::Repository), Refusal> {
        let name = int_key(request, 0).ok_or(Refusal::new(Status::BadRequest, "No repository specified"))?;
        let (group, repo) = split_repo_path(&name).ok_or(Refusal::new(Status::NotFound, "Not found"))?;
        let dir = self.root.join(group).join(repo);
        if !dir.is_dir() || !(self.allowed(group, repo, peer, Perm::Read) || self.allowed(group, repo, peer, Perm::Write)) {
            return Err(Refusal::new(Status::NotFound, "Not found"));
        }
        if !self.allowed(group, repo, peer, Perm::Write) {
            return Err(Refusal::new(Status::NotAllowed, "Not allowed"));
        }
        let opened = gix::open(&dir).map_err(|_| Refusal::new(Status::ServerError, "Repository is unreadable"))?;
        Ok((name, opened))
    }

    /// The rules of a group: the files beside it and in it, then the node's
    /// own settings.
    fn group_grants(&self, group: &str) -> access::Grants {
        let mut rules = read_rules(&self.root.join(format!("{group}.allowed")));
        rules.extend(read_rules(&self.root.join(group).join("group.allowed")));
        rules.extend(self.group_rules.get(group).into_iter().flatten().cloned());
        rules.extend(self.default_rules.iter().cloned());
        access::Grants::from_rules(&rules)
    }

    fn repo_grants(&self, group: &str, repo: &str) -> access::Grants {
        access::Grants::from_rules(&read_rules(&self.root.join(group).join(format!("{repo}.allowed"))))
    }

    /// Whether `peer` holds `perm` for a repository, as stock rngit resolves
    /// it (see [`access`]): the repository's own rules, then the group's.
    pub fn allowed(&self, group: &str, repo: &str, peer: &[u8; 16], perm: Perm) -> bool {
        access::repository(peer, perm, &self.repo_grants(group, repo), &self.group_grants(group))
    }

    /// Whether `peer` holds `perm` for a group.
    pub fn allowed_in_group(&self, group: &str, peer: &[u8; 16], perm: Perm) -> bool {
        access::in_group(peer, perm, &self.group_grants(group))
    }

    fn list(&self, request: &Value, map: &MapRef, peer: &[u8; 16]) -> Result<Response, Refusal> {
        let (name, repo) = self.readable(request, peer)?;
        if map.get("for_push").and_then(Value::as_bool).unwrap_or(false) {
            let (group, dir) = split_repo_path(&name).ok_or(Refusal::new(Status::NotFound, "Not found"))?;
            if !self.allowed(group, dir, peer, Perm::Write) {
                return Err(Refusal::new(Status::NotFound, "Not allowed"));
            }
        }
        let (refs, head) = list_refs(&repo)?;
        let mut out = String::new();
        for (id, name) in &refs {
            let _ = writeln!(out, "{id} {name}");
        }
        let _ = writeln!(out, "@{head} HEAD");
        let mut reply = vec![Status::Ok.to_byte()];
        reply.extend_from_slice(out.as_bytes());
        Ok(Response::Packet(reply))
    }

    fn fetch(&self, request: &Value, map: &MapRef, peer: &[u8; 16]) -> Result<Response, Refusal> {
        let (_, repo) = self.readable(request, peer)?;
        let wanted = map.array("refs");
        if wanted.is_empty() {
            return Err(Refusal::new(Status::BadRequest, "No refs specified"));
        }
        if wanted.len() > MAX_REFS {
            return Err(Refusal::new(Status::BadRequest, "Too many refs"));
        }
        let mut names = Vec::new();
        for entry in wanted {
            let entry = MapRef::new(entry).map_err(|_| Refusal::new(Status::BadRequest, "Invalid request data"))?;
            let name = entry
                .str("ref")
                .filter(|n| bundle::valid_ref_name(n))
                .ok_or(Refusal::new(Status::BadRequest, "Missing ref specification"))?;
            let have = entry.str("have").and_then(|h| ObjectId::from_hex(h.as_bytes()).ok());
            names.push((name, have));
        }
        let others: Vec<ObjectId> =
            map.array("have").iter().take(MAX_HAVES).filter_map(|v| ObjectId::from_hex(v.as_str()?.as_bytes()).ok()).collect();
        let wants: Vec<(&str, Option<ObjectId>)> = names.iter().map(|(n, h)| (n.as_str(), *h)).collect();
        let mut key_refs = Vec::new();
        for (name, have) in &wants {
            let id = bundle::direct_id(&repo, name).map_err(|_| Refusal::new(Status::NotFound, "Not found"))?;
            key_refs.push((name.to_string(), id, *have));
        }
        let key: CacheKey = (repo.git_dir().to_path_buf(), key_refs, others.clone());
        let cached = self.cache.lock().ok().and_then(|mut cache| cache.get(&key));
        let data = match cached {
            Some(data) => data,
            None => {
                let data = Arc::new(self.build_in_time(&repo, &wants, &others)?);
                if let Ok(mut cache) = self.cache.lock() {
                    cache.put(key, data.clone());
                }
                data
            }
        };
        if data.len() > self.max_bundle_bytes {
            return Err(Refusal::new(Status::ServerError, "Bundle is too large"));
        }
        let data = data.as_ref().clone();
        let mut metadata = Vec::new();
        rmpv::encode::write_value(&mut metadata, &Value::Map(vec![(Value::from(1), Value::from(0))]))
            .map_err(|_| Refusal::new(Status::ServerError, "Could not build bundle"))?;
        Ok(Response::File { data, metadata: Some(metadata) })
    }
}

impl Node {
    fn push(&self, request: &Value, map: &MapRef, peer: &[u8; 16]) -> Result<Response, Refusal> {
        let (_, repo) = self.writable(request, peer)?;
        let force = map.get("force").and_then(Value::as_bool).unwrap_or(false);
        if let Some(Value::Array(operations)) = map.get("operations") {
            return self.update_refs(&repo, operations);
        }
        let remote =
            map.str("remote_ref").filter(|n| pushable_ref(n)).ok_or(Refusal::new(Status::BadRequest, "Missing ref specification"))?;
        let data = map.bytes("bundle").ok_or(Refusal::new(Status::BadRequest, "Invalid request data"))?;
        if data.len() > self.quotas.max_push_bytes {
            return Err(Refusal::new(Status::BadRequest, "Bundle is too large"));
        }
        if dir_size(repo.git_dir()).saturating_add(data.len() as u64) > self.quotas.max_repo_bytes {
            return Err(Refusal::new(Status::ServerError, "Repository is full"));
        }
        let unverified = || Refusal::new(Status::ServerError, "Could not verify bundle");
        let tips = self.unpack_in_time(&repo, data).map_err(|_| unverified())?;
        let (new, _) = tips.first().ok_or_else(unverified)?;
        self.set_ref(&repo, &remote, *new, force).map_err(|_| unverified())?;
        Ok(ack())
    }

    /// The bundle-less push: point refs at objects the repository has.
    fn update_refs(&self, repo: &gix::Repository, operations: &[Value]) -> Result<Response, Refusal> {
        if operations.len() > MAX_OPERATIONS {
            return Err(Refusal::new(Status::BadRequest, "Too many operations"));
        }
        let mut plan = Vec::new();
        for operation in operations {
            let operation = MapRef::new(operation).map_err(|_| Refusal::new(Status::BadRequest, "Invalid request data"))?;
            if operation.str("action").as_deref() != Some("update_ref") {
                return Err(Refusal::new(Status::BadRequest, "Invalid request data"));
            }
            let name =
                operation.str("ref").filter(|n| pushable_ref(n)).ok_or(Refusal::new(Status::BadRequest, "Missing ref specification"))?;
            let sha = operation.str("sha").ok_or(Refusal::new(Status::BadRequest, "Invalid request data"))?;
            let id = ObjectId::from_hex(sha.as_bytes()).map_err(|_| Refusal::new(Status::BadRequest, "Invalid request data"))?;
            if repo.find_object(id).is_err() {
                return Err(Refusal::new(Status::ServerError, format!("Object {id} does not exist in repository")));
            }
            plan.push((name, id, operation.get("force").and_then(Value::as_bool).unwrap_or(false)));
        }
        // All of them are checked before any is moved.
        for (name, id, force) in &plan {
            self.may_move(repo, name, *id, *force).map_err(|_| Refusal::new(Status::ServerError, "Could not update ref"))?;
        }
        for (name, id, force) in &plan {
            self.set_ref(repo, name, *id, *force).map_err(|_| Refusal::new(Status::ServerError, "Could not update ref"))?;
        }
        Ok(ack())
    }

    /// Whether `name` may be pointed at `id`: a new ref, a branch moved
    /// forward, the same place, or anything when forced.
    fn may_move(&self, repo: &gix::Repository, name: &str, id: ObjectId, force: bool) -> Result<(), ()> {
        let Ok(current) = bundle::direct_id(repo, name) else {
            return Ok(());
        };
        if force || current == id {
            return Ok(());
        }
        // A tag stays where it is; a branch moves forward.
        if name.starts_with("refs/heads/") && bundle::is_ancestor(repo, current, id).unwrap_or(false) { Ok(()) } else { Err(()) }
    }

    fn set_ref(&self, repo: &gix::Repository, name: &str, id: ObjectId, force: bool) -> Result<(), ()> {
        use gix::refs::transaction::PreviousValue;
        // A branch has to be a commit.
        if name.starts_with("refs/heads/") && repo.find_object(id).map_err(drop)?.kind != gix::object::Kind::Commit {
            return Err(());
        }
        self.may_move(repo, name, id, force)?;
        let previous = match bundle::direct_id(repo, name) {
            Ok(current) if !force => PreviousValue::MustExistAndMatch(gix::refs::Target::Object(current)),
            Ok(_) => PreviousValue::Any,
            Err(_) => PreviousValue::MustNotExist,
        };
        repo.reference(name, id, previous, "rngit push").map(drop).map_err(drop)
    }

    fn unpack_in_time(&self, repo: &gix::Repository, data: &[u8]) -> Result<Vec<(ObjectId, String)>, ()> {
        let (stop, done) = std::sync::mpsc::channel::<()>();
        let interrupt = AtomicBool::new(false);
        let (limit, flag, pack) = (self.max_request_time, &interrupt, self.quotas.pack);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                if done.recv_timeout(limit).is_err() {
                    flag.store(true, Ordering::Relaxed);
                }
            });
            let unpacked = bundle::ingest_objects_checked(repo, data, pack, flag);
            let _ = stop.send(());
            unpacked.map_err(drop)
        })
    }

    fn delete_ref(&self, request: &Value, map: &MapRef, peer: &[u8; 16]) -> Result<Response, Refusal> {
        let (_, repo) = self.writable(request, peer)?;
        let name = map.str("ref").filter(|n| pushable_ref(n)).ok_or(Refusal::new(Status::BadRequest, "Invalid request"))?;
        // Deleting what isn't there is fine.
        if let Ok(reference) = repo.find_reference(name.as_str()) {
            reference.delete().map_err(|_| Refusal::new(Status::ServerError, "Could not delete ref"))?;
        }
        Ok(ack())
    }

    /// Make a bare repository, whose creator is its admin.
    fn create(&self, request: &Value, peer: &[u8; 16]) -> Result<Response, Refusal> {
        let name = int_key(request, 0).ok_or(Refusal::new(Status::BadRequest, "No repository specified"))?;
        let (group, repo) = split_repo_path(&name).ok_or(Refusal::new(Status::BadRequest, "Invalid repository name"))?;
        let group_dir = self.root.join(group);
        let may = self.allowed_in_group(group, peer, Perm::Create);
        // Someone who didn't say who they are would be made admin of it, as anyone.
        if !group_dir.is_dir() {
            return Err(Refusal::new(Status::NotFound, "Not found"));
        }
        if !may || *peer == NULL_IDENT {
            return Err(Refusal::new(Status::NotAllowed, "Not allowed"));
        }
        let dir = group_dir.join(repo);
        if dir.exists() {
            return Err(Refusal::new(Status::NotAllowed, "Repository already exists"));
        }
        let kept = std::fs::read_dir(&group_dir).map_or(0, |entries| entries.flatten().filter(|e| e.path().is_dir()).count());
        if kept >= self.quotas.max_repos_per_group {
            return Err(Refusal::new(Status::ServerError, "Too many repositories in this group"));
        }
        let failed = || Refusal::new(Status::ServerError, "Could not create repository");
        gix::init_bare(&dir).map_err(|_| failed())?;
        // The default branch, whatever this computer's git is set to.
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/master\n").map_err(|_| failed())?;
        let allowed = group_dir.join(format!("{repo}.allowed"));
        std::fs::write(allowed, format!("adm:{}", hex::encode(peer))).map_err(|_| failed())?;
        Ok(ack())
    }

    /// Build a bundle, giving up once [`Node::max_request_time`] has passed.
    fn build_in_time(&self, repo: &gix::Repository, wants: &[(&str, Option<ObjectId>)], others: &[ObjectId]) -> Result<Vec<u8>, Refusal> {
        let (stop, done) = std::sync::mpsc::channel::<()>();
        let interrupt = AtomicBool::new(false);
        let (limit, flag) = (self.max_request_time, &interrupt);
        let built = std::thread::scope(|scope| {
            scope.spawn(move || {
                if done.recv_timeout(limit).is_err() {
                    flag.store(true, Ordering::Relaxed);
                }
            });
            let built = bundle::build_many_until(repo, wants, others, flag);
            let _ = stop.send(());
            built
        });
        match built {
            Ok(data) => Ok(data),
            Err(_) if interrupt.load(Ordering::Relaxed) => Err(Refusal::new(Status::ServerError, "Took too long")),
            Err(_) => Err(Refusal::new(Status::ServerError, "Could not build bundle")),
        }
    }
}

/// The reply to a request that succeeded and has nothing to say. rsReticulum
/// sends a reply that is one whole MessagePack value as that value rather
/// than as bytes, and a lone `0x00` is the number 0, which a client takes as
/// no reply at all. A second byte makes it bytes; clients read only the first.
fn ack() -> Response {
    Response::Packet(vec![Status::Ok.to_byte(), 0])
}

/// A ref a push may make, move or delete: a branch or a tag.
fn pushable_ref(name: &str) -> bool {
    (name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) && bundle::valid_ref_name(name)
}

/// Bytes under `path`, counting at most so many files so a deep tree can't take long.
fn dir_size(path: &Path) -> u64 {
    let (mut total, mut seen) = (0u64, 0usize);
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > 500_000 {
                return u64::MAX;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// `group/name.git` split in two, if both parts are plain folder names.
pub fn split_repo_path(path: &str) -> Option<(&str, &str)> {
    let (group, repo) = path.split_once('/')?;
    let plain = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && !s.starts_with('.')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (plain(group) && plain(repo) && repo.ends_with(".git") && repo.len() > 4).then_some((group, repo))
}

fn int_key(request: &Value, key: u64) -> Option<String> {
    match request {
        Value::Map(entries) => entries.iter().find(|(k, _)| k.as_u64() == Some(key)).and_then(|(_, v)| v.as_str()).map(str::to_string),
        _ => None,
    }
}

fn read_rules(path: &Path) -> Vec<Rule> {
    use std::io::Read as _;
    let Ok(file) = std::fs::File::open(path) else { return Vec::new() };
    let mut text = String::new();
    // Past the cap the rest is ignored, so a huge file can only take permissions away.
    let _ = file.take(MAX_RULES_BYTES).read_to_string(&mut text);
    Rule::parse(&text)
}

/// Every branch and tag with the object it names, and the branch HEAD is on.
fn list_refs(repo: &gix::Repository) -> Result<(Vec<(ObjectId, String)>, String), Refusal> {
    let broken = Refusal::new(Status::ServerError, "Repository is unreadable");
    let platform = repo.references().map_err(|_| Refusal::new(Status::ServerError, "Repository is unreadable"))?;
    let mut refs = Vec::new();
    for reference in platform.all().map_err(|_| Refusal::new(Status::ServerError, "Repository is unreadable"))?.flatten() {
        let name = reference.name().as_bstr().to_string();
        if !(name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) {
            continue;
        }
        if let gix::refs::TargetRef::Object(id) = reference.target() {
            refs.push((id.to_owned(), name));
        }
        if refs.len() >= MAX_REFS {
            break;
        }
    }
    refs.sort_by(|a, b| a.1.cmp(&b.1));
    let head = match repo.head_name() {
        Ok(Some(name)) => name.as_bstr().to_string(),
        Ok(None) => "refs/heads/master".to_string(),
        Err(_) => return Err(broken),
    };
    Ok((refs, head))
}
