//! Reading a repository on this computer, such as the copy `mirror_into`
//! keeps: its history, the files in a branch or tag, and one file's text.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: ObjectId,
    pub author: String,
    /// Seconds since the epoch, as the commit says.
    pub time: i64,
    /// The first line of the message.
    pub subject: String,
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

fn commit_of<'r>(repo: &'r gix::Repository, refname: &str) -> Result<gix::Commit<'r>> {
    if !refname.starts_with("refs/") {
        return Err(Error::InvalidInput(format!("{refname:?} is not a full ref name")));
    }
    let mut reference = repo.find_reference(refname).map_err(|_| Error::Git(format!("no such ref: {refname}")))?;
    let id = reference.peel_to_id().map_err(Error::git)?;
    id.object().map_err(Error::git)?.peel_to_commit().map_err(Error::git)
}

fn shorten(text: &str) -> String {
    text.chars().take(MAX_LINE).collect()
}

/// The newest commits reachable from a branch or tag, newest first, at most `limit`.
pub fn log(dir: &Path, refname: &str, limit: usize) -> Result<Vec<CommitInfo>> {
    let repo = gix::open(dir).map_err(Error::git)?;
    let tip = commit_of(&repo, refname)?.id;
    let mut out = Vec::new();
    let walk = repo.rev_walk([tip]).sorting(gix::revision::walk::Sorting::ByCommitTime(Default::default())).all().map_err(Error::git)?;
    for info in walk.take(limit.min(MAX_LOG)) {
        let info = info.map_err(Error::git)?;
        let commit = info.object().map_err(Error::git)?;
        let message = commit.message_raw_sloppy().to_string();
        let author = commit.author().map_err(Error::git)?;
        out.push(CommitInfo {
            id: info.id,
            author: shorten(&author.name.to_string()),
            time: author.time().map(|t| t.seconds).unwrap_or(0),
            subject: shorten(message.lines().next().unwrap_or_default()),
        });
    }
    Ok(out)
}

/// Where in the tree of `refname` the path `path` is: a folder, or a file
/// at the end. `path` has no leading slash and is empty for the top.
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

/// What is in a folder of a branch or tag: folders first, then files, each by name.
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

/// A file of a branch or tag: its size, and its text up to `max_bytes` if it is text.
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
