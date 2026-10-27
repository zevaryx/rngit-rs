//! The rngit client: one method per request the node understands.

use std::path::Path;

use gix::ObjectId;
use rmpv::Value;

use crate::bundle::{self, Haves};
use crate::error::{Error, Result};
use crate::sign::{self, Manifest, ManifestInput, Signer};
use crate::transport::{Response, Transport};
use crate::url::RepoUrl;
use crate::wire::{self, MapRef, Request};

/// Caps on everything that comes back from a node.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Largest git bundle accepted from a fetch.
    pub max_bundle_bytes: usize,
    /// Largest release artifact accepted.
    pub max_artifact_bytes: usize,
    /// Largest text or MessagePack reply (ref lists, release lists, documents).
    pub max_reply_bytes: usize,
    /// Most refs read from a ref list.
    pub max_refs: usize,
    /// Most entries read from any list in a reply.
    pub max_list_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_bundle_bytes: 512 * 1024 * 1024,
            max_artifact_bytes: 512 * 1024 * 1024,
            max_reply_bytes: 16 * 1024 * 1024,
            max_refs: 100_000,
            max_list_entries: 10_000,
        }
    }
}

/// What [`Client::push_all`] did, by full ref name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushReport {
    /// Branches and tags sent to the node, or moved there.
    pub pushed: Vec<String>,
    /// Refs the node already had at the same object.
    pub up_to_date: Vec<String>,
    /// Branches the node has ahead of, or apart from, ours. Left alone unless forced.
    pub rejected: Vec<String>,
    /// Refs removed from the node because the copy no longer has them.
    pub deleted: Vec<String>,
}

/// The refs a node advertises.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRefs {
    pub refs: Vec<(ObjectId, String)>,
    /// The ref HEAD points at, if the node said.
    pub head: Option<String>,
}

impl RemoteRefs {
    pub fn get(&self, name: &str) -> Option<ObjectId> {
        self.refs.iter().find(|(_, n)| n == name).map(|(id, _)| *id)
    }
}

/// One ref to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub sha: ObjectId,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseSummary {
    pub tag: String,
    pub hash: String,
    pub created: u64,
    pub status: String,
    pub created_by: String,
    pub preview: String,
    pub format: String,
    pub artifacts: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseList {
    pub releases: Vec<ReleaseSummary>,
    pub latest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactInfo {
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseDetail {
    pub tag: String,
    pub hash: String,
    pub created: u64,
    pub status: String,
    pub created_by: String,
    pub notes: String,
    pub notes_format: String,
    pub artifacts: Vec<ArtifactInfo>,
}

/// Which list of work documents to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkScope {
    Active,
    Completed,
    Proposed,
    All,
}

impl WorkScope {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkScope::Active => "active",
            WorkScope::Completed => "completed",
            WorkScope::Proposed => "proposed",
            WorkScope::All => "all",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkSummary {
    pub id: u64,
    pub title: String,
    pub created: f64,
    pub edited: f64,
    pub author: String,
    pub format: String,
    pub comments: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkList {
    pub active: Vec<WorkSummary>,
    pub completed: Vec<WorkSummary>,
    pub proposed: Vec<WorkSummary>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkDoc {
    pub id: u64,
    pub scope: String,
    pub content: String,
    /// Comments exactly as the node sent them. Their layout isn't documented.
    pub comments: Vec<Value>,
    pub title: String,
    pub created: f64,
    pub edited: f64,
    pub author: String,
    pub format: String,
    /// The author's 64-byte public key.
    pub identity: Option<[u8; 64]>,
    pub signature: Option<[u8; 64]>,
    /// True when the content's signature checked out against `identity`.
    pub verified: bool,
}

/// Where a new work document goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    /// Straight onto the active list.
    Create,
    /// Onto the proposed list, for callers with the `p` permission.
    Propose,
}

fn check_ref_name(name: &str) -> Result<()> {
    if bundle::valid_ref_name(name) { Ok(()) } else { Err(Error::InvalidInput(format!("{name:?} is not a full ref name"))) }
}

/// Reject names that could escape a directory or confuse a path.
pub fn check_artifact_name(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || name.chars().any(char::is_control);
    if bad { Err(Error::InvalidInput(format!("unsafe artifact name {name:?}"))) } else { Ok(()) }
}

pub struct Client<T> {
    transport: T,
    limits: Limits,
}

impl<T: Transport> Client<T> {
    pub fn new(transport: T) -> Self {
        Client { transport, limits: Limits::default() }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn transport(&self) -> &T {
        &self.transport
    }

    async fn packet(&self, path: &str, request: Request) -> Result<Vec<u8>> {
        match self.transport.request(path, request.encode()).await? {
            Response::Packet(reply) => {
                if reply.len() > self.limits.max_reply_bytes {
                    return Err(Error::TooLarge { what: "reply", size: reply.len(), limit: self.limits.max_reply_bytes });
                }
                Ok(wire::split_reply(&reply)?.to_vec())
            }
            Response::File { .. } => Err(Error::Protocol(format!("{path} answered with a file"))),
        }
    }

    async fn packet_map(&self, path: &str, request: Request) -> Result<Value> {
        wire::decode(&self.packet(path, request).await?)
    }

    async fn file(&self, path: &str, request: Request, limit: usize, what: &'static str) -> Result<Vec<u8>> {
        match self.transport.request(path, request.encode()).await? {
            Response::File { data, .. } => {
                if data.len() > limit {
                    return Err(Error::TooLarge { what, size: data.len(), limit });
                }
                Ok(data)
            }
            // Errors come back as a normal status reply.
            Response::Packet(reply) => {
                wire::split_reply(&reply)?;
                Err(Error::Protocol(format!("{path} answered with a plain reply instead of a file")))
            }
        }
    }

    // --- git transport -----------------------------------------------------

    /// List a repository's refs and its HEAD.
    pub async fn list(&self, repo: &RepoUrl, for_push: bool) -> Result<RemoteRefs> {
        let body = self.packet("/git/list", Request::repo(&repo.repo_path()).put("for_push", for_push)).await?;
        parse_ref_list(&body, self.limits.max_refs)
    }

    /// Download a bundle holding the wanted refs, in full.
    pub async fn fetch_bundle(&self, repo: &RepoUrl, wants: &[Want]) -> Result<Vec<u8>> {
        self.fetch_bundle_with(repo, wants, &Haves::default()).await
    }

    /// Download a bundle holding the wanted refs, leaving out what `haves`
    /// says the caller already holds. The node then sends a thin pack.
    pub async fn fetch_bundle_with(&self, repo: &RepoUrl, wants: &[Want], haves: &Haves) -> Result<Vec<u8>> {
        if wants.is_empty() {
            return Err(Error::InvalidInput("nothing to fetch".into()));
        }
        let refs = wants
            .iter()
            .map(|w| {
                let mut entry =
                    vec![(Value::from("sha"), Value::from(w.sha.to_string())), (Value::from("ref"), Value::from(w.name.as_str()))];
                if let Some((_, have)) = haves.per_ref.iter().find(|(name, _)| *name == w.name) {
                    entry.push((Value::from("have"), Value::from(have.to_string())));
                }
                Value::Map(entry)
            })
            .collect::<Vec<_>>();
        let mut request = Request::repo(&repo.repo_path()).put("refs", Value::Array(refs));
        if !haves.tips.is_empty() {
            request = request.put("have", Value::Array(haves.tips.iter().map(|id| Value::from(id.to_string())).collect()));
        }
        self.file("/git/fetch", request, self.limits.max_bundle_bytes, "bundle").await
    }

    /// What `dest` already holds, if it is a repository.
    fn haves_in(wants: &[Want], dest: &Path) -> Haves {
        match gix::open(dest) {
            Ok(repository) => bundle::local_haves(&repository, &wants.iter().map(|w| w.name.as_str()).collect::<Vec<_>>()),
            Err(_) => Haves::default(),
        }
    }

    /// Fetch the wanted refs into the repository at `dest` (made bare if it
    /// doesn't exist). Only what `dest` lacks is downloaded. Returns the refs
    /// that were set.
    pub async fn fetch_into(&self, repo: &RepoUrl, wants: &[Want], dest: &Path) -> Result<Vec<(ObjectId, String)>> {
        let data = self.fetch_bundle_with(repo, wants, &Self::haves_in(wants, dest)).await?;
        let repository = bundle::open_or_init_bare(dest)?;
        bundle::ingest(&repository, &data)
    }

    /// Like [`Client::fetch_into`] but only adds objects: no ref is written.
    /// This is what a git remote helper needs, as git sets the refs itself.
    pub async fn fetch_objects(&self, repo: &RepoUrl, wants: &[Want], dest: &Path) -> Result<Vec<(ObjectId, String)>> {
        let data = self.fetch_bundle_with(repo, wants, &Self::haves_in(wants, dest)).await?;
        let repository = gix::open(dest).map_err(Error::git)?;
        bundle::ingest_objects(&repository, &data)
    }

    /// Fetch every branch and tag the node lists, plus HEAD, into `dest`.
    pub async fn clone_into(&self, repo: &RepoUrl, dest: &Path) -> Result<RemoteRefs> {
        let remote = self.list(repo, false).await?;
        let wants: Vec<Want> = remote
            .refs
            .iter()
            .filter(|(_, name)| name.starts_with("refs/heads/") || name.starts_with("refs/tags/"))
            .map(|(sha, name)| Want { sha: *sha, name: name.clone() })
            .collect();
        if !wants.is_empty() {
            self.fetch_into(repo, &wants, dest).await?;
        } else {
            bundle::open_or_init_bare(dest)?;
        }
        if let (Some(head), Ok(repository)) = (&remote.head, gix::open(dest)) {
            set_head(&repository, head)?;
        }
        Ok(remote)
    }

    /// Make `dest` a copy of the repository: clone it, or bring an existing
    /// copy up to date. A branch or tag the node no longer has is removed
    /// from the copy too. Returns the node's refs and how many were removed.
    pub async fn mirror_into(&self, repo: &RepoUrl, dest: &Path) -> Result<(RemoteRefs, usize)> {
        let remote = self.clone_into(repo, dest).await?;
        let repository = gix::open(dest).map_err(Error::git)?;
        let removed = prune_refs(&repository, &remote)?;
        Ok((remote, removed))
    }

    /// Send every branch and tag of the repository at `source` to the node.
    /// A branch that isn't a fast-forward is listed as rejected and left as
    /// it is, unless `force` is set. With `prune`, a branch or tag the node
    /// has and `source` doesn't is deleted there, so the node ends up with
    /// the same refs as the copy. Stops at the first error, such as missing
    /// permission; refs already sent stay sent.
    pub async fn push_all(&self, repo: &RepoUrl, source: &Path, force: bool, prune: bool) -> Result<PushReport> {
        let remote = self.list(repo, true).await?;
        let mut local = Vec::new();
        {
            let repository = gix::open(source).map_err(Error::git)?;
            let platform = repository.references().map_err(Error::git)?;
            for prefix in ["refs/heads/", "refs/tags/"] {
                for reference in platform.prefixed(prefix).map_err(Error::git)?.flatten() {
                    let name = reference.name().as_bstr().to_string();
                    // A name the node would refuse is skipped here rather than failing the whole push.
                    if check_ref_name(&name).is_ok() {
                        local.push((bundle::direct_id(&repository, &name)?, name));
                    }
                    if local.len() > self.limits.max_refs {
                        return Err(Error::TooLarge { what: "ref list", size: local.len(), limit: self.limits.max_refs });
                    }
                }
            }
        }
        let mut report = PushReport::default();
        for (id, name) in &local {
            if remote.get(name) == Some(*id) {
                report.up_to_date.push(name.clone());
                continue;
            }
            match self.push_from_with(repo, source, name, name, force, &remote).await {
                Ok(()) => report.pushed.push(name.clone()),
                Err(Error::NonFastForward) => report.rejected.push(name.clone()),
                Err(e) => return Err(e),
            }
        }
        if prune {
            for (_, name) in &remote.refs {
                let branch_or_tag = name.starts_with("refs/heads/") || name.starts_with("refs/tags/");
                if branch_or_tag && !local.iter().any(|(_, n)| n == name) {
                    self.delete_ref(repo, name).await?;
                    report.deleted.push(name.clone());
                }
            }
        }
        Ok(report)
    }

    /// Send a bundle for one ref. See [`Client::push_from`] for the usual route.
    pub async fn push_bundle(&self, repo: &RepoUrl, local_ref: &str, remote_ref: &str, force: bool, bundle: Vec<u8>) -> Result<()> {
        for name in [local_ref, remote_ref] {
            if !name.starts_with("refs/") {
                return Err(Error::InvalidInput(format!("{name:?} is not a full ref name")));
            }
        }
        let request = Request::repo(&repo.repo_path())
            .put("local_ref", local_ref)
            .put("remote_ref", remote_ref)
            .put("force", force)
            .bin("bundle", bundle);
        self.packet("/git/push", request).await.map(drop)
    }

    /// Delete a branch or tag on the node. Deleting a ref that isn't there is
    /// not an error, and the node does not protect the default branch.
    pub async fn delete_ref(&self, repo: &RepoUrl, remote_ref: &str) -> Result<()> {
        check_ref_name(remote_ref)?;
        self.packet("/git/delete", Request::repo(&repo.repo_path()).put("ref", remote_ref)).await.map(drop)
    }

    /// Point a ref at an object the node already has, without sending a
    /// bundle. The node answers `0xff` if it doesn't have the object.
    pub async fn update_ref(&self, repo: &RepoUrl, remote_ref: &str, sha: ObjectId, force: bool) -> Result<()> {
        check_ref_name(remote_ref)?;
        let operation = Value::Map(vec![
            (Value::from("action"), Value::from("update_ref")),
            (Value::from("ref"), Value::from(remote_ref)),
            (Value::from("sha"), Value::from(sha.to_string())),
            (Value::from("force"), Value::from(force)),
        ]);
        self.packet("/git/push", Request::repo(&repo.repo_path()).put("operations", Value::Array(vec![operation]))).await.map(drop)
    }

    /// Push `local_ref` from the repository at `source` to `remote_ref`.
    /// Without `force`, a push that isn't a fast-forward is refused here
    /// before anything is sent, since the node's reply to it can't be told
    /// from a corrupt bundle.
    pub async fn push_from(&self, repo: &RepoUrl, source: &Path, local_ref: &str, remote_ref: &str, force: bool) -> Result<()> {
        let remote = self.list(repo, true).await?;
        self.push_from_with(repo, source, local_ref, remote_ref, force, &remote).await
    }

    /// Like [`Client::push_from`] with a ref list the caller already has from
    /// `list(repo, true)`, so a batch of pushes asks the node for it once.
    ///
    /// When the node already holds the object being pushed (some ref there
    /// points at it or at a commit that has it in its history), no bundle is sent: the ref is moved with `update_ref`,
    /// as stock rngit does.
    pub async fn push_from_with(
        &self,
        repo: &RepoUrl,
        source: &Path,
        local_ref: &str,
        remote_ref: &str,
        force: bool,
        remote: &RemoteRefs,
    ) -> Result<()> {
        for name in [local_ref, remote_ref] {
            check_ref_name(name)?;
        }
        let have = remote.get(remote_ref);
        let (direct, data) = {
            let repository = gix::open(source).map_err(Error::git)?;
            if let (Some(old), false) = (have, force) {
                let tip = bundle::resolve_ref(&repository, local_ref)?;
                if !bundle::is_ancestor(&repository, old, tip)? {
                    return Err(Error::NonFastForward);
                }
            }
            let direct = bundle::direct_id(&repository, local_ref)?;
            let tips: Vec<ObjectId> = remote.refs.iter().map(|(sha, _)| *sha).collect();
            let held = bundle::held_by_remote(&repository, direct, &tips);
            (direct, if held { None } else { Some(bundle::build(&repository, local_ref, have)?) })
        };
        match data {
            None => self.update_ref(repo, remote_ref, direct, force).await,
            Some(data) => self.push_bundle(repo, local_ref, remote_ref, force, data).await,
        }
    }

    /// Create an empty repository. Needs `c` on the group.
    pub async fn create(&self, repo: &RepoUrl) -> Result<()> {
        self.packet("/git/create", Request::repo(&repo.repo_path())).await.map(drop)
    }

    /// Make the node fork a repository from `source` (an rns, https or ssh URL).
    pub async fn fork(&self, repo: &RepoUrl, source: &str) -> Result<()> {
        self.packet("/git/fork", Request::repo(&repo.repo_path()).put("source", source)).await.map(drop)
    }

    /// Make the node mirror a repository from `source`.
    pub async fn mirror(&self, repo: &RepoUrl, source: &str) -> Result<()> {
        self.packet("/git/mirror", Request::repo(&repo.repo_path()).put("source", source)).await.map(drop)
    }

    /// Ask the node to pull the latest from a fork's or mirror's source.
    pub async fn sync(&self, repo: &RepoUrl) -> Result<()> {
        self.packet("/git/sync", Request::repo(&repo.repo_path())).await.map(drop)
    }

    // --- permissions -------------------------------------------------------

    /// Read a repository's `.allowed` file. Needs `adm`.
    pub async fn repo_perms(&self, repo: &RepoUrl) -> Result<String> {
        let request = Request::repo(&repo.repo_path()).put("operation", "rperms").put("step", "get");
        perms_content(&self.packet_map("/mgmt/perms", request).await?)
    }

    pub async fn set_repo_perms(&self, repo: &RepoUrl, content: &str) -> Result<()> {
        let request = Request::repo(&repo.repo_path()).put("operation", "rperms").put("step", "set").put("content", content);
        self.packet("/mgmt/perms", request).await.map(drop)
    }

    /// Read a group's `group.allowed` file. The group name goes in key 2.
    pub async fn group_perms(&self, group: &str) -> Result<String> {
        let request = Request::group(group).put("operation", "gperms").put("step", "get");
        perms_content(&self.packet_map("/mgmt/perms", request).await?)
    }

    pub async fn set_group_perms(&self, group: &str, content: &str) -> Result<()> {
        let request = Request::group(group).put("operation", "gperms").put("step", "set").put("content", content);
        self.packet("/mgmt/perms", request).await.map(drop)
    }

    // --- releases ----------------------------------------------------------

    fn release_request(repo: &RepoUrl, operation: &str) -> Request {
        Request::repo(&repo.repo_path()).put("operation", operation)
    }

    pub async fn releases(&self, repo: &RepoUrl) -> Result<ReleaseList> {
        let value = self.packet_map("/mgmt/release", Self::release_request(repo, "list")).await?;
        // A repository with no releases gets a bare empty array, not a map.
        if matches!(&value, Value::Array(items) if items.is_empty()) {
            return Ok(ReleaseList { releases: Vec::new(), latest: None });
        }
        let map = MapRef::new(&value)?;
        let releases = map
            .array("releases")
            .iter()
            .take(self.limits.max_list_entries)
            .filter_map(|v| {
                let m = MapRef::new(v).ok()?;
                Some(ReleaseSummary {
                    tag: m.str("tag")?,
                    hash: m.str("hash").unwrap_or_default(),
                    created: m.u64("created").unwrap_or(0),
                    status: m.str("status").unwrap_or_default(),
                    created_by: m.str("created_by").unwrap_or_default(),
                    preview: m.str("preview").unwrap_or_default(),
                    format: m.str("format").unwrap_or_default(),
                    artifacts: m.u64("artifacts").unwrap_or(0),
                })
            })
            .collect();
        Ok(ReleaseList { releases, latest: map.str("latest") })
    }

    pub async fn release(&self, repo: &RepoUrl, tag: &str) -> Result<ReleaseDetail> {
        let value = self.packet_map("/mgmt/release", Self::release_request(repo, "view").put("tag", tag)).await?;
        let m = MapRef::new(&value)?;
        let artifacts = m
            .array("artifacts")
            .iter()
            .take(self.limits.max_list_entries)
            .filter_map(|v| {
                let a = MapRef::new(v).ok()?;
                Some(ArtifactInfo { name: a.str("name")?, size: a.u64("size").unwrap_or(0) })
            })
            .collect();
        Ok(ReleaseDetail {
            tag: m.str("tag").unwrap_or_else(|| tag.to_string()),
            hash: m.str("hash").unwrap_or_default(),
            created: m.u64("created").unwrap_or(0),
            status: m.str("status").unwrap_or_default(),
            created_by: m.str("created_by").unwrap_or_default(),
            notes: m.str("notes").unwrap_or_default(),
            notes_format: m.str("notes_format").unwrap_or_default(),
            artifacts,
        })
    }

    /// Download one artifact. `tag` may be `"latest"`.
    pub async fn fetch_artifact(&self, repo: &RepoUrl, tag: &str, artifact: &str) -> Result<Vec<u8>> {
        check_artifact_name(artifact)?;
        let request = Self::release_request(repo, "fetch").put("tag", tag).put("artifact", artifact);
        self.file("/mgmt/release", request, self.limits.max_artifact_bytes, "artifact").await
    }

    /// Download a release's manifest and every artifact it lists, and verify
    /// all signatures. The caller decides whether to trust the signer.
    pub async fn fetch_verified_release(&self, repo: &RepoUrl, tag: &str) -> Result<(Manifest, Vec<(String, Vec<u8>)>)> {
        let rsm = self.fetch_artifact(repo, tag, "manifest.rsm").await?;
        let manifest = sign::verify_manifest(&rsm)?;
        let mut files = Vec::new();
        for artifact in &manifest.artifacts {
            files.push((artifact.name.clone(), self.fetch_artifact(repo, tag, &artifact.name).await?));
        }
        let manifest = sign::verify_release_files(&rsm, &files)?;
        Ok((manifest, files))
    }

    pub async fn delete_release(&self, repo: &RepoUrl, tag: &str) -> Result<()> {
        self.packet("/mgmt/release", Self::release_request(repo, "delete").put("tag", tag)).await.map(drop)
    }

    /// Publish a release: the tag must already be pushed. Signs each file,
    /// builds the manifest, uploads everything and finalizes. `released` is
    /// the ISO 8601 time shown in the manifest and `now` is a Unix time.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_release(
        &self,
        repo: &RepoUrl,
        signer: &dyn Signer,
        tag: &str,
        commit: Option<&str>,
        notes: &str,
        files: &[(String, Vec<u8>)],
        released: &str,
        now: u64,
    ) -> Result<()> {
        for (name, _) in files {
            check_artifact_name(name)?;
            if name == "manifest.rsm" || name.ends_with(".rsg") {
                return Err(Error::InvalidInput(format!("{name} is reserved for signatures")));
            }
        }
        let step = |s: &str| Self::release_request(repo, "create").put("step", s).put("tag", tag);
        let init = step("init").put("hash", commit.map_or(Value::Nil, Value::from)).put("notes", notes).put("notes_format", "markdown");
        self.packet("/mgmt/release", init).await?;

        let mut signed = Vec::new();
        for (name, data) in files {
            signed.push((name.clone(), sign::make_rsg(signer, data, now)?));
        }
        let origin = repo.destination;
        let rsm = sign::make_rsm(
            signer,
            &ManifestInput {
                name: &repo.dir_name(),
                version: tag,
                released,
                origin,
                path: &repo.repo_path(),
                commit: commit.unwrap_or_default(),
                message: notes.as_bytes(),
                artifacts: &signed,
            },
            now,
        )?;
        // Stock uploads each file and its signature first, and the manifest last.
        let mut uploads: Vec<(String, Vec<u8>)> = Vec::new();
        for ((name, data), (_, rsg)) in files.iter().zip(signed) {
            uploads.push((name.clone(), data.clone()));
            uploads.push((format!("{name}.rsg"), rsg));
        }
        uploads.push(("manifest.rsm".into(), rsm));
        for (name, data) in uploads {
            let request = step("artifact").put("artifact_name", name).bin("artifact_data", data);
            self.packet("/mgmt/release", request).await?;
        }
        self.packet("/mgmt/release", step("finalize")).await.map(drop)
    }

    // --- work documents ----------------------------------------------------

    fn work_request(repo: &RepoUrl, operation: &str) -> Request {
        Request::repo(&repo.repo_path()).put("operation", operation)
    }

    pub async fn work_list(&self, repo: &RepoUrl, scope: WorkScope) -> Result<WorkList> {
        let value = self.packet_map("/mgmt/work", Self::work_request(repo, "list").put("scope", scope.as_str())).await?;
        let m = MapRef::new(&value)?;
        let read = |key: &str| -> Vec<WorkSummary> {
            m.array(key)
                .iter()
                .take(self.limits.max_list_entries)
                .filter_map(|v| {
                    let d = MapRef::new(v).ok()?;
                    Some(WorkSummary {
                        id: d.u64("id")?,
                        title: d.str("title").unwrap_or_default(),
                        created: d.f64("created").unwrap_or(0.0),
                        edited: d.f64("edited").unwrap_or(0.0),
                        author: wire::hex_or_empty(d.bytes("author")).if_empty_then(|| d.str("author").unwrap_or_default()),
                        format: d.str("format").unwrap_or_default(),
                        comments: d.u64("comments").unwrap_or(0),
                    })
                })
                .collect()
        };
        Ok(WorkList { active: read("active"), completed: read("completed"), proposed: read("proposed") })
    }

    /// Create or propose a work document. The content is signed with `signer`.
    /// Returns the new id and the scope it landed in.
    pub async fn work_create(
        &self,
        repo: &RepoUrl,
        signer: &dyn Signer,
        kind: WorkKind,
        title: &str,
        content: &str,
        format: &str,
    ) -> Result<(u64, String)> {
        let operation = match kind {
            WorkKind::Create => "create",
            WorkKind::Propose => "propose",
        };
        let signature = signer.sign(content.as_bytes())?;
        let request = Self::work_request(repo, operation)
            .put("title", title)
            .put("content", content)
            .put("format", format)
            .bin("signature", signature.to_vec());
        let value = self.packet_map("/mgmt/work", request).await?;
        let m = MapRef::new(&value)?;
        Ok((m.u64("id").ok_or_else(|| Error::Protocol("reply has no id".into()))?, m.str("scope").unwrap_or_default()))
    }

    /// Read a document and check its signature against the author's key.
    pub async fn work_view(&self, repo: &RepoUrl, doc_id: u64, scope: WorkScope) -> Result<WorkDoc> {
        let request = Self::work_request(repo, "view").put("doc_id", doc_id).put("scope", scope.as_str());
        let value = self.packet_map("/mgmt/work", request).await?;
        let m = MapRef::new(&value)?;
        let meta = m.map("meta").ok_or_else(|| Error::Protocol("document has no meta".into()))?;
        let content = m.str("content").unwrap_or_default();
        let identity: Option<[u8; 64]> = meta.bytes("identity").and_then(|b| b.try_into().ok());
        let signature: Option<[u8; 64]> = meta.bytes("signature").and_then(|b| b.try_into().ok());
        let verified = match (identity, signature) {
            (Some(key), Some(sig)) => sign::verify_signature(&key, content.as_bytes(), &sig).is_ok(),
            _ => false,
        };
        Ok(WorkDoc {
            id: m.u64("id").unwrap_or(doc_id),
            scope: m.str("scope").unwrap_or_default(),
            content,
            comments: m.array("comments").iter().take(self.limits.max_list_entries).cloned().collect(),
            title: meta.str("title").unwrap_or_default(),
            created: meta.f64("created").unwrap_or(0.0),
            edited: meta.f64("edited").unwrap_or(0.0),
            author: wire::hex_or_empty(meta.bytes("author")).if_empty_then(|| meta.str("author").unwrap_or_default()),
            format: meta.str("format").unwrap_or_default(),
            identity,
            signature,
            verified,
        })
    }

    /// Add a comment. Returns the comment id.
    pub async fn work_comment(&self, repo: &RepoUrl, doc_id: u64, scope: WorkScope, content: &str, format: &str) -> Result<u64> {
        let request = Self::work_request(repo, "comment")
            .put("doc_id", doc_id)
            .put("scope", scope.as_str())
            .put("content", content)
            .put("format", format);
        let value = self.packet_map("/mgmt/work", request).await?;
        MapRef::new(&value)?.u64("id").ok_or_else(|| Error::Protocol("reply has no id".into()))
    }

    /// Replace a document's content and title. Only its author may.
    pub async fn work_edit(
        &self,
        repo: &RepoUrl,
        signer: &dyn Signer,
        doc_id: u64,
        scope: WorkScope,
        title: &str,
        content: &str,
    ) -> Result<()> {
        let signature = signer.sign(content.as_bytes())?;
        let request = Self::work_request(repo, "edit")
            .put("doc_id", doc_id)
            .put("scope", scope.as_str())
            .put("content", content)
            .put("title", title)
            .bin("signature", signature.to_vec());
        self.packet("/mgmt/work", request).await.map(drop)
    }

    async fn work_move(&self, repo: &RepoUrl, operation: &str, doc_id: u64) -> Result<(u64, String)> {
        let value = self.packet_map("/mgmt/work", Self::work_request(repo, operation).put("doc_id", doc_id)).await?;
        let m = MapRef::new(&value)?;
        Ok((m.u64("id").unwrap_or(doc_id), m.str("scope").unwrap_or_default()))
    }

    pub async fn work_complete(&self, repo: &RepoUrl, doc_id: u64) -> Result<(u64, String)> {
        self.work_move(repo, "complete", doc_id).await
    }

    pub async fn work_activate(&self, repo: &RepoUrl, doc_id: u64) -> Result<(u64, String)> {
        self.work_move(repo, "activate", doc_id).await
    }

    pub async fn work_delete(&self, repo: &RepoUrl, doc_id: u64, scope: WorkScope) -> Result<()> {
        let request = Self::work_request(repo, "delete").put("doc_id", doc_id).put("scope", scope.as_str());
        self.packet("/mgmt/work", request).await.map(drop)
    }

    /// Read a document's `.allowed` file.
    pub async fn work_perms(&self, repo: &RepoUrl, doc_id: u64) -> Result<String> {
        let request = Self::work_request(repo, "perms").put("doc_id", doc_id).put("step", "get");
        perms_content(&self.packet_map("/mgmt/work", request).await?)
    }

    pub async fn set_work_perms(&self, repo: &RepoUrl, doc_id: u64, content: &str) -> Result<()> {
        let request = Self::work_request(repo, "perms").put("doc_id", doc_id).put("step", "set").put("content", content);
        self.packet("/mgmt/work", request).await.map(drop)
    }
}

trait IfEmpty {
    fn if_empty_then(self, f: impl FnOnce() -> String) -> String;
}

impl IfEmpty for String {
    fn if_empty_then(self, f: impl FnOnce() -> String) -> String {
        if self.is_empty() { f() } else { self }
    }
}

fn perms_content(value: &Value) -> Result<String> {
    MapRef::new(value)?.str("content").ok_or_else(|| Error::Protocol("reply has no content".into()))
}

/// Point a repository's HEAD at a branch, if the branch is a valid ref name.
/// Remove the branches and tags of `repo` that `remote` doesn't list.
/// Returns how many were removed.
pub fn prune_refs(repo: &gix::Repository, remote: &RemoteRefs) -> Result<usize> {
    let mut stale = Vec::new();
    let platform = repo.references().map_err(Error::git)?;
    for prefix in ["refs/heads/", "refs/tags/"] {
        for reference in platform.prefixed(prefix).map_err(Error::git)?.flatten() {
            let name = reference.name().as_bstr().to_string();
            if remote.get(&name).is_none() {
                stale.push(name);
            }
        }
    }
    for name in &stale {
        let edit = gix::refs::transaction::RefEdit {
            change: gix::refs::transaction::Change::Delete {
                expected: gix::refs::transaction::PreviousValue::Any,
                log: gix::refs::transaction::RefLog::AndReference,
            },
            name: name.as_str().try_into().map_err(Error::git)?,
            deref: false,
        };
        repo.edit_reference(edit).map_err(Error::git)?;
    }
    Ok(stale.len())
}

fn set_head(repo: &gix::Repository, target: &str) -> Result<()> {
    if !target.starts_with("refs/heads/") {
        return Ok(());
    }
    let name = gix::refs::FullName::try_from(target).map_err(Error::git)?;
    repo.edit_reference(gix::refs::transaction::RefEdit {
        change: gix::refs::transaction::Change::Update {
            log: gix::refs::transaction::LogChange::default(),
            expected: gix::refs::transaction::PreviousValue::Any,
            new: gix::refs::Target::Symbolic(name),
        },
        name: "HEAD".try_into().map_err(Error::git)?,
        deref: false,
    })
    .map_err(Error::git)?;
    Ok(())
}

/// Parse the text `/git/list` returns. Bad lines are an error, not skipped:
/// a half-read ref list could make a push look like a fast-forward.
pub fn parse_ref_list(body: &[u8], max_refs: usize) -> Result<RemoteRefs> {
    let text = std::str::from_utf8(body).map_err(|_| Error::Protocol("ref list is not UTF-8".into()))?;
    let mut out = RemoteRefs::default();
    for line in text.lines().filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix('@') {
            let (target, name) = rest.rsplit_once(' ').ok_or_else(|| Error::Protocol(format!("bad symref line {line:?}")))?;
            if name == "HEAD" {
                out.head = Some(target.to_string());
            }
            continue;
        }
        let (sha, name) = line.split_once(' ').ok_or_else(|| Error::Protocol(format!("bad ref line {line:?}")))?;
        let id = ObjectId::from_hex(sha.as_bytes()).map_err(|_| Error::Protocol(format!("bad object id in {line:?}")))?;
        if out.refs.len() >= max_refs {
            return Err(Error::TooLarge { what: "ref list", size: out.refs.len() + 1, limit: max_refs });
        }
        out.refs.push((id, name.to_string()));
    }
    Ok(out)
}
