//! Serving repositories to rngit clients.
//!
//! [`Node`] answers the requests of the protocol from repositories in a
//! folder, with no network in it: the caller hands over a request path, its
//! body and who sent it, and sends back what [`Node::handle`] returns. This
//! first part reads only: `/git/list` and `/git/fetch`. Everything else is
//! answered "not allowed", as a node would answer a client with no write
//! permission.
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

/// The hash stock rngit gives a peer that didn't identify itself.
pub const NULL_IDENT: [u8; 16] = [0xd7, 0xdb, 0x22, 0xf6, 0x3b, 0x45, 0x3c, 0x23, 0xbb, 0x06, 0x88, 0xdd, 0xe5, 0x65, 0xb7, 0xc1];

/// Most refs listed or asked for in one request.
const MAX_REFS: usize = 10_000;
/// Most hashes a client may say it already holds.
const MAX_HAVES: usize = 1024;
/// Largest request body read.
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Largest `.allowed` file read.
const MAX_RULES_BYTES: u64 = 64 * 1024;

/// What an identity may do. `adm` includes all of them.
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
            "r" => vec![Perm::Read],
            "w" => vec![Perm::Write],
            "rw" => vec![Perm::Read, Perm::Write],
            "c" => vec![Perm::Create],
            "s" => vec![Perm::Stats],
            "rel" => vec![Perm::Release],
            "i" => vec![Perm::Issues],
            "p" => vec![Perm::Proposals],
            "adm" => vec![Perm::Admin],
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
            let who = match who.trim() {
                "all" => Who::Everyone,
                "none" => Who::Nobody,
                hash => match decode_identity(hash) {
                    Some(id) => Who::Identity(id),
                    None => continue,
                },
            };
            let perms: Option<Vec<Perm>> = words.split_whitespace().map(Perm::from_word).collect::<Option<Vec<_>>>().map(|p| p.concat());
            if let Some(perms) = perms.filter(|p| !p.is_empty()) {
                rules.push(Rule { perms, who });
            }
        }
        rules
    }

    fn gives(&self, who: &[u8; 16], perm: Perm) -> bool {
        let for_them = match self.who {
            Who::Everyone => true,
            Who::Nobody => false,
            Who::Identity(id) => &id == who,
        };
        for_them && (self.perms.contains(&perm) || self.perms.contains(&Perm::Admin))
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
    /// How long building one bundle may take.
    pub max_request_time: Duration,
    /// Bundles built lately, so a second clone of the same thing costs nothing.
    cache: Arc<Mutex<BundleCache>>,
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
struct Refusal(Status, &'static str);

impl Refusal {
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
        let peer = peer.unwrap_or(NULL_IDENT);
        let result = (|| {
            if self.blocked.contains(&peer) {
                return Err(Refusal(Status::NotAllowed, "Not allowed"));
            }
            if data.len() > MAX_REQUEST_BYTES {
                return Err(Refusal(Status::BadRequest, "Request is too large"));
            }
            let request = wire::decode(data).map_err(|_| Refusal(Status::BadRequest, "Invalid request"))?;
            let map = MapRef::new(&request).map_err(|_| Refusal(Status::BadRequest, "Invalid request"))?;
            match path {
                "/git/list" => self.list(&request, &map, &peer),
                "/git/fetch" => self.fetch(&request, &map, &peer),
                _ => Err(Refusal(Status::NotAllowed, "Not allowed")),
            }
        })();
        Some(result.unwrap_or_else(|refusal| refusal.reply()))
    }

    /// The repository a request names, if the peer may read it. A repository
    /// the peer may not read looks the same as one that isn't there.
    fn readable(&self, request: &Value, peer: &[u8; 16]) -> Result<(String, gix::Repository), Refusal> {
        let name = int_key(request, 0).ok_or(Refusal(Status::BadRequest, "No repository specified"))?;
        let (group, repo) = split_repo_path(&name).ok_or(Refusal(Status::NotFound, "Not found"))?;
        let dir = self.root.join(group).join(repo);
        if !dir.is_dir() || !self.allowed(group, repo, peer, Perm::Read) {
            return Err(Refusal(Status::NotFound, "Not found"));
        }
        let opened = gix::open(&dir).map_err(|_| Refusal(Status::ServerError, "Repository is unreadable"))?;
        Ok((name, opened))
    }

    /// Whether `peer` holds `perm` for a repository: its own rules first, then
    /// the group's file, then the node's settings.
    pub fn allowed(&self, group: &str, repo: &str, peer: &[u8; 16], perm: Perm) -> bool {
        let repo_rules = read_rules(&self.root.join(group).join(format!("{repo}.allowed")));
        let group_rules = read_rules(&self.root.join(group).join("group.allowed"));
        let node_rules = self.group_rules.get(group).map_or(&[][..], Vec::as_slice);
        [repo_rules, group_rules, node_rules.to_vec(), self.default_rules.clone()].iter().flatten().any(|rule| rule.gives(peer, perm))
    }

    fn list(&self, request: &Value, map: &MapRef, peer: &[u8; 16]) -> Result<Response, Refusal> {
        let (name, repo) = self.readable(request, peer)?;
        if map.get("for_push").and_then(Value::as_bool).unwrap_or(false) {
            let (group, dir) = split_repo_path(&name).ok_or(Refusal(Status::NotFound, "Not found"))?;
            if !self.allowed(group, dir, peer, Perm::Write) {
                return Err(Refusal(Status::NotFound, "Not allowed"));
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
            return Err(Refusal(Status::BadRequest, "No refs specified"));
        }
        if wanted.len() > MAX_REFS {
            return Err(Refusal(Status::BadRequest, "Too many refs"));
        }
        let mut names = Vec::new();
        for entry in wanted {
            let entry = MapRef::new(entry).map_err(|_| Refusal(Status::BadRequest, "Invalid request data"))?;
            let name =
                entry.str("ref").filter(|n| bundle::valid_ref_name(n)).ok_or(Refusal(Status::BadRequest, "Missing ref specification"))?;
            let have = entry.str("have").and_then(|h| ObjectId::from_hex(h.as_bytes()).ok());
            names.push((name, have));
        }
        let others: Vec<ObjectId> =
            map.array("have").iter().take(MAX_HAVES).filter_map(|v| ObjectId::from_hex(v.as_str()?.as_bytes()).ok()).collect();
        let wants: Vec<(&str, Option<ObjectId>)> = names.iter().map(|(n, h)| (n.as_str(), *h)).collect();
        let mut key_refs = Vec::new();
        for (name, have) in &wants {
            let id = bundle::direct_id(&repo, name).map_err(|_| Refusal(Status::NotFound, "Not found"))?;
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
            return Err(Refusal(Status::ServerError, "Bundle is too large"));
        }
        let data = data.as_ref().clone();
        let mut metadata = Vec::new();
        rmpv::encode::write_value(&mut metadata, &Value::Map(vec![(Value::from(1), Value::from(0))]))
            .map_err(|_| Refusal(Status::ServerError, "Could not build bundle"))?;
        Ok(Response::File { data, metadata: Some(metadata) })
    }
}

impl Node {
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
            Err(_) if interrupt.load(Ordering::Relaxed) => Err(Refusal(Status::ServerError, "Took too long")),
            Err(_) => Err(Refusal(Status::ServerError, "Could not build bundle")),
        }
    }
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
    let broken = Refusal(Status::ServerError, "Repository is unreadable");
    let platform = repo.references().map_err(|_| Refusal(Status::ServerError, "Repository is unreadable"))?;
    let mut refs = Vec::new();
    for reference in platform.all().map_err(|_| Refusal(Status::ServerError, "Repository is unreadable"))?.flatten() {
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
