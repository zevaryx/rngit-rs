//! Reading a repository on this computer, such as the copy `mirror_into`
//! keeps or one a node hosts: its history, one commit and what it changed,
//! the files in a branch, tag or commit, and one file's text.
//!
//! Nothing here runs a program or writes to the repository. What it reads
//! may have come from a node, so names, counts and sizes are capped, and a
//! path that tries to leave the tree is refused.

use std::path::Path;

use gix::ObjectId;

use crate::error::{Error, Result};

/// Most commits `log` returns.
pub const MAX_LOG: usize = 1000;
/// Most entries `tree` returns for one folder.
pub const MAX_ENTRIES: usize = 5000;
/// Longest path (in bytes) and deepest folder looked up.
const MAX_PATH: usize = 1024;
const MAX_DEPTH: usize = 64;
/// Longest commit subject and author name kept (in characters).
const MAX_LINE: usize = 200;
/// Most of a commit message kept (in bytes).
const MAX_MESSAGE: usize = 16 * 1024;
/// Most commits a log walks past to reach the ones asked for.
pub const MAX_SKIP: usize = 20_000;
/// Most changes listed for one commit, and most tree entries compared to find them.
pub const MAX_CHANGES: usize = 1000;
const MAX_COMPARED: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub author: String,
    /// Seconds since the epoch, as the commit says.
    pub time: i64,
    /// The first line of the message.
    pub subject: String,
}

/// A commit, with all of its message and what it changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDetail {
    pub info: CommitInfo,
    pub email: String,
    /// The whole message, cut at 16 KiB.
    pub message: String,
    pub parents: Vec<ObjectId>,
    /// What it changed from its first parent (all its files, for a first commit), by path.
    pub changes: Vec<Change>,
    /// Whether it changed more than is listed.
    pub more_changes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Link,
    /// A folder that is another repository; its content isn't here.
    Submodule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    /// A file's size in bytes.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileView {
    pub size: u64,
    /// The text, up to the limit asked for, if the file is text (valid UTF-8, no NUL bytes).
    pub text: Option<String>,
    /// Whether the text stops before the end of the file.
    pub truncated: bool,
}

/// The branches and tags of a copy, and the branch its HEAD is on, as the node listed them.
pub fn refs(dir: &Path) -> Result<crate::client::RemoteRefs> {
    let repo = gix::open(dir).map_err(Error::git)?;
    let mut out = crate::client::RemoteRefs::default();
    let platform = repo.references().map_err(Error::git)?;
    for prefix in ["refs/heads/", "refs/tags/"] {
        for reference in platform.prefixed(prefix).map_err(Error::git)?.flatten() {
            if out.refs.len() >= MAX_ENTRIES {
                return Err(Error::TooLarge { what: "ref list", size: out.refs.len() + 1, limit: MAX_ENTRIES });
            }
            let name = reference.name().as_bstr().to_string();
            out.refs.push((crate::bundle::direct_id(&repo, &name)?, name));
        }
    }
    out.head = repo.find_reference("HEAD").ok().and_then(|head| match head.target() {
        gix::refs::TargetRef::Symbolic(name) => Some(name.as_bstr().to_string()),
        gix::refs::TargetRef::Object(_) => None,
    });
    Ok(out)
}

/// The commit a full ref name, or a commit's whole hash, is at.
fn commit_of<'r>(repo: &'r gix::Repository, refname: &str) -> Result<gix::Commit<'r>> {
    if let Ok(id) = ObjectId::from_hex(refname.as_bytes()) {
        let object = repo.find_object(id).map_err(|_| Error::Git(format!("no such commit: {refname}")))?;
        return object.peel_to_commit().map_err(Error::git);
    }
    if !refname.starts_with("refs/") {
        return Err(Error::InvalidInput(format!("{refname:?} is not a full ref name or a commit's hash")));
    }
    let mut reference = repo.find_reference(refname).map_err(|_| Error::Git(format!("no such ref: {refname}")))?;
    let id = reference.peel_to_id().map_err(Error::git)?;
    id.object().map_err(Error::git)?.peel_to_commit().map_err(Error::git)
}

fn shorten(text: &str) -> String {
    text.chars().take(MAX_LINE).collect()
}

fn info_of(commit: &gix::Commit<'_>) -> Result<CommitInfo> {
    let message = commit.message_raw_sloppy().to_string();
    let author = commit.author().map_err(Error::git)?;
    Ok(CommitInfo {
        id: commit.id,
        author: shorten(&author.name.to_string()),
        time: author.time().map(|t| t.seconds).unwrap_or(0),
        subject: shorten(message.lines().next().unwrap_or_default()),
    })
}

/// The newest commits reachable from a branch or tag, newest first, at most `limit`.
pub fn log(dir: &Path, refname: &str, limit: usize) -> Result<Vec<CommitInfo>> {
    log_page(dir, refname, 0, limit).map(|(commits, _)| commits)
}

/// Like [`log`], from the `skip`th newest on (at most [`MAX_SKIP`] are
/// skipped), and whether there are older ones. `refname` may also be a commit's hash.
pub fn log_page(dir: &Path, refname: &str, skip: usize, limit: usize) -> Result<(Vec<CommitInfo>, bool)> {
    let repo = gix::open(dir).map_err(Error::git)?;
    let tip = commit_of(&repo, refname)?.id;
    let limit = limit.min(MAX_LOG);
    let mut out = Vec::new();
    let walk = repo.rev_walk([tip]).sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default())).all().map_err(Error::git)?;
    let mut walk = walk.skip(skip.min(MAX_SKIP));
    for info in walk.by_ref().take(limit) {
        let info = info.map_err(Error::git)?;
        out.push(info_of(&info.object().map_err(Error::git)?)?);
    }
    Ok((out, limit > 0 && walk.next().is_some()))
}

/// One commit, by a full ref name or its hash, with what it changed.
pub fn commit(dir: &Path, refname: &str) -> Result<CommitDetail> {
    let repo = gix::open(dir).map_err(Error::git)?;
    let commit = commit_of(&repo, refname)?;
    let info = info_of(&commit)?;
    let email = shorten(&commit.author().map_err(Error::git)?.email.to_string());
    let raw = commit.message_raw_sloppy();
    let message = String::from_utf8_lossy(&raw[..raw.len().min(MAX_MESSAGE)]).into_owned();
    let parents: Vec<ObjectId> = commit.parent_ids().map(|id| id.detach()).collect();
    let before = match parents.first() {
        Some(parent) => Some(repo.find_commit(*parent).map_err(Error::git)?.tree_id().map_err(Error::git)?.detach()),
        None => None,
    };
    let after = commit.tree_id().map_err(Error::git)?.detach();
    let mut compare = Compare { repo: &repo, changes: Vec::new(), compared: 0, more: false };
    compare.trees(before, Some(after), "", 0)?;
    let Compare { changes, more, .. } = compare;
    Ok(CommitDetail { info, email, message, parents, changes, more_changes: more })
}

/// What changed between two trees, found by walking both side by side.
struct Compare<'r> {
    repo: &'r gix::Repository,
    changes: Vec<Change>,
    compared: usize,
    more: bool,
}

impl Compare<'_> {
    /// The entries of a tree by name: whether each is a folder, and its object.
    fn entries(&mut self, tree: Option<ObjectId>) -> Result<std::collections::BTreeMap<String, (bool, ObjectId)>> {
        let mut out = std::collections::BTreeMap::new();
        let Some(id) = tree else { return Ok(out) };
        for entry in self.repo.find_tree(id).map_err(Error::git)?.iter() {
            let entry = entry.map_err(Error::git)?;
            self.compared += 1;
            if self.compared > MAX_COMPARED {
                self.more = true;
                break;
            }
            let folder = entry.mode().kind() == gix::object::tree::EntryKind::Tree;
            out.insert(entry.filename().to_string(), (folder, entry.oid().to_owned()));
        }
        Ok(out)
    }

    fn add(&mut self, path: String, kind: ChangeKind) {
        if self.changes.len() >= MAX_CHANGES {
            self.more = true;
        } else {
            self.changes.push(Change { path, kind });
        }
    }

    fn trees(&mut self, before: Option<ObjectId>, after: Option<ObjectId>, prefix: &str, depth: usize) -> Result<()> {
        if self.more {
            return Ok(());
        }
        if depth > MAX_DEPTH {
            self.more = true;
            return Ok(());
        }
        let (old, new) = (self.entries(before)?, self.entries(after)?);
        let mut names: Vec<&String> = old.keys().chain(new.keys()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let (was, is) = (old.get(name).copied(), new.get(name).copied());
            if was == is {
                continue;
            }
            // A folder on either side is walked; a file on either side is a change.
            let folder = |side: Option<(bool, ObjectId)>| side.filter(|(folder, _)| *folder).map(|(_, id)| id);
            let file = |side: Option<(bool, ObjectId)>| side.filter(|(folder, _)| !*folder).map(|(_, id)| id);
            if folder(was).is_some() || folder(is).is_some() {
                self.trees(folder(was), folder(is), &path, depth + 1)?;
            }
            match (file(was), file(is)) {
                (Some(_), Some(_)) => self.add(path.clone(), ChangeKind::Modified),
                (Some(_), None) => self.add(path.clone(), ChangeKind::Deleted),
                (None, Some(_)) => self.add(path.clone(), ChangeKind::Added),
                (None, None) => {}
            }
            if self.more {
                break;
            }
        }
        Ok(())
    }
}

/// Where in the tree of `refname` (or a commit's hash) the path `path` is:
/// a folder, or a file at the end. `path` has no leading slash and is empty for the top.
fn lookup<'r>(repo: &'r gix::Repository, refname: &str, path: &str) -> Result<Option<gix::object::tree::Entry<'r>>> {
    if path.len() > MAX_PATH || path.split('/').count() > MAX_DEPTH {
        return Err(Error::InvalidInput("that path is too long".into()));
    }
    if !path.is_empty() && path.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.contains('\0')) {
        return Err(Error::InvalidInput(format!("{path:?} is not a path in the repository")));
    }
    let tree = commit_of(repo, refname)?.tree().map_err(Error::git)?;
    if path.is_empty() {
        return Ok(None);
    }
    match tree.lookup_entry_by_path(path).map_err(Error::git)? {
        Some(entry) => Ok(Some(entry)),
        None => Err(Error::Git(format!("no such path: {path}"))),
    }
}

fn kind_of(mode: gix::object::tree::EntryMode) -> EntryKind {
    match mode.kind() {
        gix::object::tree::EntryKind::Tree => EntryKind::Dir,
        gix::object::tree::EntryKind::Link => EntryKind::Link,
        gix::object::tree::EntryKind::Commit => EntryKind::Submodule,
        _ => EntryKind::File,
    }
}

/// What is in a folder of a branch, tag or commit: folders first, then files, each by name.
/// `path` is empty for the top.
pub fn tree(dir: &Path, refname: &str, path: &str) -> Result<Vec<TreeEntry>> {
    let repo = gix::open(dir).map_err(Error::git)?;
    let folder = match lookup(&repo, refname, path)? {
        None => commit_of(&repo, refname)?.tree().map_err(Error::git)?,
        Some(entry) if kind_of(entry.mode()) == EntryKind::Dir => entry.object().map_err(Error::git)?.into_tree(),
        Some(_) => return Err(Error::InvalidInput(format!("{path:?} is a file, not a folder"))),
    };
    let mut out = Vec::new();
    for entry in folder.iter() {
        let entry = entry.map_err(Error::git)?;
        if out.len() >= MAX_ENTRIES {
            return Err(Error::TooLarge { what: "folder", size: out.len() + 1, limit: MAX_ENTRIES });
        }
        let kind = kind_of(entry.mode());
        let size = (kind == EntryKind::File).then(|| repo.find_header(entry.oid()).ok().map(|h| h.size())).flatten();
        out.push(TreeEntry { name: shorten(&entry.filename().to_string()), kind, size });
    }
    out.sort_by(|a, b| (a.kind != EntryKind::Dir, &a.name).cmp(&(b.kind != EntryKind::Dir, &b.name)));
    Ok(out)
}

/// A file of a branch, tag or commit: its size, and its text up to `max_bytes` if it is text.
pub fn file(dir: &Path, refname: &str, path: &str, max_bytes: usize) -> Result<FileView> {
    let repo = gix::open(dir).map_err(Error::git)?;
    let entry = lookup(&repo, refname, path)?.ok_or_else(|| Error::InvalidInput("that is the top folder, not a file".into()))?;
    if !matches!(kind_of(entry.mode()), EntryKind::File | EntryKind::Link) {
        return Err(Error::InvalidInput(format!("{path:?} is not a file")));
    }
    let size = repo.find_header(entry.object_id()).map_err(Error::git)?.size();
    // Read no more than asked (a little over, so a cut can be told from the end).
    let blob = entry.object().map_err(Error::git)?;
    let data = &blob.data;
    let shown = &data[..data.len().min(max_bytes)];
    // A cut in the middle of a character is not a reason to call it binary.
    let valid = match std::str::from_utf8(shown) {
        Ok(text) => Some(text),
        Err(e) if e.error_len().is_none() && shown.len() < data.len() => std::str::from_utf8(&shown[..e.valid_up_to()]).ok(),
        Err(_) => None,
    };
    let text = valid.filter(|text| !text.contains('\0')).map(str::to_string);
    Ok(FileView { size, truncated: text.is_some() && shown.len() < data.len(), text })
}

/// What `path` is in a branch, tag or commit: `None` for the top folder.
pub fn kind(dir: &Path, refname: &str, path: &str) -> Result<Option<EntryKind>> {
    let repo = gix::open(dir).map_err(Error::git)?;
    Ok(lookup(&repo, refname, path)?.map(|entry| kind_of(entry.mode())))
}

/// A whole file of a branch, tag or commit, if it is at most `max_bytes`
/// (it isn't read if it's bigger).
pub fn read(dir: &Path, refname: &str, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
    let repo = gix::open(dir).map_err(Error::git)?;
    let entry = lookup(&repo, refname, path)?.ok_or_else(|| Error::InvalidInput("that is the top folder, not a file".into()))?;
    if !matches!(kind_of(entry.mode()), EntryKind::File | EntryKind::Link) {
        return Err(Error::InvalidInput(format!("{path:?} is not a file")));
    }
    let size = repo.find_header(entry.object_id()).map_err(Error::git)?.size();
    if size > max_bytes as u64 {
        return Err(Error::TooLarge { what: "file", size: usize::try_from(size).unwrap_or(usize::MAX), limit: max_bytes });
    }
    Ok(entry.object().map_err(Error::git)?.detach().data)
}
