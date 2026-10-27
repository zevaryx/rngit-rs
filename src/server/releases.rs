//! Releases on a node: `/mgmt/release`.
//!
//! A release of `group/name.git` lives in `group/name.git.releases/<tag>/`,
//! as stock rngit keeps it, so a folder can be served by either:
//!
//! - `META`: `key = value` lines (tag, hash, created, status, created_by);
//! - `RELEASE.md` or `RELEASE.mu`: the notes;
//! - `artifacts/`: the files, with their `.rsg` signatures and `manifest.rsm`
//!   (the node stores them and checks nothing: clients verify);
//! - `THANKS`: a MessagePack map with a count, kept for stock's pages.
//!
//! `latest` in the releases folder names the newest published release.
//! A release is made in steps (`init`, then `artifact` for each file, then
//! `finalize`) and can only be written while it is a draft.

use rmpv::Value;

use super::support::{now, packed, refuse, text, write_file};
use super::{Node, Perm, Refusal, ack, dir_size, int_key, split_repo_path};
use crate::error::Status;
use crate::transport::Response;
use crate::wire::MapRef;

/// Most releases listed in one reply.
const MAX_RELEASES: usize = 1000;
/// Largest `META` file read.
const MAX_META_BYTES: u64 = 16 * 1024;
/// Longest notes read back, or taken.
const MAX_NOTES_BYTES: usize = 256 * 1024;

/// A tag as a release folder's name: letters, digits and `._-+`, and not
/// a name that means a folder up.
fn plain_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 100
        && !tag.starts_with('.')
        && tag.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

/// An artifact's name as a file's: a single plain name, nothing hidden.
fn plain_artifact(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
}

/// The `META` of a release, as lines of `key = value`.
fn read_meta(release: &std::path::Path) -> Option<Vec<(String, String)>> {
    use std::io::Read as _;
    let mut text = String::new();
    std::fs::File::open(release.join("META")).ok()?.take(MAX_META_BYTES).read_to_string(&mut text).ok()?;
    Some(
        text.lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                Some((key.trim().to_string(), value.trim().trim_matches(['"', '\'']).to_string()))
            })
            .collect(),
    )
}

fn meta_get<'a>(meta: &'a [(String, String)], key: &str) -> Option<&'a str> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn write_meta(release: &std::path::Path, meta: &[(String, String)]) -> std::io::Result<()> {
    let text: String = meta.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
    write_file(&release.join("META"), text.as_bytes())
}

/// The notes of a release, and their format.
fn read_notes(release: &std::path::Path) -> Option<(String, &'static str)> {
    use std::io::Read as _;
    for (file, format) in [("RELEASE.md", "markdown"), ("RELEASE.mu", "micron"), ("RELEASE.txt", "text")] {
        let Ok(opened) = std::fs::File::open(release.join(file)) else { continue };
        let mut text = String::new();
        // Notes that aren't text are no notes.
        return opened.take(MAX_NOTES_BYTES as u64).read_to_string(&mut text).ok().map(|_| (text, format));
    }
    None
}

/// The artifacts of a release: names and sizes, by name.
fn artifacts(release: &std::path::Path) -> Vec<(String, u64)> {
    let mut found: Vec<(String, u64)> = std::fs::read_dir(release.join("artifacts"))
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let meta = e.metadata().ok().filter(std::fs::Metadata::is_file)?;
                    let name = e.file_name().into_string().ok().filter(|n| plain_artifact(n))?;
                    Some((name, meta.len()))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found.truncate(1000);
    found
}

impl Node {
    /// `/mgmt/release`: list, view and fetch (read), create, delete and
    /// `latest` (release). Everyone must say who they are.
    pub(super) fn release(&self, request: &Value, map: &MapRef, peer: &[u8; 16]) -> Result<Response, Refusal> {
        if *peer == super::NULL_IDENT {
            return Err(refuse(Status::NotAllowed, "Not identified"));
        }
        let name = int_key(request, 0).ok_or(refuse(Status::BadRequest, "No repository specified"))?;
        let operation = map.str("operation").ok_or(refuse(Status::BadRequest, "Invalid request"))?;
        let Some((group, repo)) = split_repo_path(&name) else { return Err(refuse(Status::NotFound, "Not found")) };
        let dir = self.root.join(group).join(repo);
        if !dir.is_dir() || !self.allowed(group, repo, peer, Perm::Read) {
            return Err(refuse(Status::NotFound, "Not found"));
        }
        let changes = matches!(operation.as_str(), "create" | "delete" | "latest");
        if !matches!(operation.as_str(), "list" | "view" | "fetch") && !changes {
            return Err(refuse(Status::BadRequest, "Invalid request"));
        }
        if changes && !self.allowed(group, repo, peer, Perm::Release) {
            return Err(refuse(Status::NotAllowed, "Not allowed"));
        }
        let releases = self.root.join(group).join(format!("{repo}.releases"));
        match operation.as_str() {
            "list" => self.release_list(&releases),
            "view" => self.release_view(&releases, map),
            "fetch" => self.release_fetch(&releases, map),
            "create" => match map.str("step").as_deref() {
                Some("init") => self.release_init(&releases, &dir, map, peer),
                Some("artifact") => self.release_artifact(&releases, map),
                Some("finalize") => self.release_finalize(&releases, map),
                _ => Err(refuse(Status::BadRequest, "Invalid request")),
            },
            "delete" => release_delete(&releases, map),
            _ => release_latest(&releases, map),
        }
    }

    fn release_list(&self, releases: &std::path::Path) -> Result<Response, Refusal> {
        let Ok(entries) = std::fs::read_dir(releases) else { return packed(&Value::Array(Vec::new())) };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(meta) = path.is_dir().then(|| read_meta(&path)).flatten() else { continue };
            let Some(folder) = entry.file_name().into_string().ok().filter(|n| plain_tag(n)) else { continue };
            let tag = meta_get(&meta, "tag").filter(|t| plain_tag(t)).unwrap_or(&folder).to_string();
            let (notes, format) = read_notes(&path).unwrap_or_default();
            // What stock shows: the notes without their headings and quotes.
            let preview: Vec<&str> = notes.lines().filter(|l| !l.starts_with('#') && !l.starts_with('>')).collect();
            let created = meta_get(&meta, "created").and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
            found.push((created, tag, meta, preview.join("\n").trim().to_string(), format, artifacts(&path).len()));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        found.truncate(MAX_RELEASES);
        let published = |tag: &str| found.iter().any(|f| f.1 == tag && meta_get(&f.2, "status") == Some("published"));
        let latest = std::fs::read_to_string(releases.join("latest")).ok().map(|t| t.trim().to_string()).filter(|t| published(t));
        let list: Vec<Value> = found
            .iter()
            .map(|(created, tag, meta, preview, format, count)| {
                Value::Map(vec![
                    (text("tag"), text(tag)),
                    (text("hash"), text(meta_get(meta, "hash").unwrap_or_default())),
                    (text("created"), Value::from(*created)),
                    (text("status"), text(meta_get(meta, "status").unwrap_or("unknown"))),
                    (text("created_by"), text(meta_get(meta, "created_by").unwrap_or_default())),
                    (text("preview"), text(preview)),
                    (text("format"), text(if format.is_empty() { "markdown" } else { format })),
                    (text("artifacts"), Value::from(*count as u64)),
                ])
            })
            .collect();
        packed(&Value::Map(vec![(text("releases"), Value::Array(list)), (text("latest"), latest.map_or(Value::Nil, |t| text(&t)))]))
    }

    /// The tag a request names, with `latest` resolved.
    fn release_tag(releases: &std::path::Path, map: &MapRef) -> Result<String, Refusal> {
        let tag =
            map.str("tag").filter(|t| !t.is_empty() && !t.contains('/')).ok_or(refuse(Status::BadRequest, "Invalid tag specified"))?;
        if tag != "latest" {
            return plain_tag(&tag).then_some(tag).ok_or(refuse(Status::NotFound, "Release not found"));
        }
        let named = std::fs::read_to_string(releases.join("latest")).ok().map(|t| t.trim().to_string());
        named.filter(|t| plain_tag(t)).ok_or(refuse(Status::NotFound, "No latest release found"))
    }

    fn release_view(&self, releases: &std::path::Path, map: &MapRef) -> Result<Response, Refusal> {
        let tag = Self::release_tag(releases, map)?;
        let dir = releases.join(&tag);
        if !dir.is_dir() {
            return Err(refuse(Status::NotFound, "Release not found"));
        }
        let meta = read_meta(&dir).ok_or(refuse(Status::ServerError, "Error getting release data"))?;
        let (notes, format) = read_notes(&dir).unwrap_or_default();
        let list: Vec<Value> = artifacts(&dir)
            .into_iter()
            .map(|(name, size)| Value::Map(vec![(text("name"), text(&name)), (text("size"), Value::from(size))]))
            .collect();
        let created = meta_get(&meta, "created").and_then(|c| c.parse::<u64>().ok()).unwrap_or(0);
        packed(&Value::Map(vec![
            (text("tag"), text(meta_get(&meta, "tag").unwrap_or(&tag))),
            (text("hash"), text(meta_get(&meta, "hash").unwrap_or_default())),
            (text("created"), Value::from(created)),
            (text("status"), text(meta_get(&meta, "status").unwrap_or("unknown"))),
            (text("created_by"), text(meta_get(&meta, "created_by").unwrap_or_default())),
            (text("notes"), text(&notes)),
            (text("notes_format"), text(if format.is_empty() { "text" } else { format })),
            (text("artifacts"), Value::Array(list)),
            (text("thanks"), Value::from(0)),
        ]))
    }

    fn release_fetch(&self, releases: &std::path::Path, map: &MapRef) -> Result<Response, Refusal> {
        let tag = Self::release_tag(releases, map)?;
        let artifact = map.str("artifact").filter(|a| plain_artifact(a)).ok_or(refuse(Status::BadRequest, "Invalid artifact specified"))?;
        let file = releases.join(&tag).join("artifacts").join(&artifact);
        let meta =
            std::fs::metadata(&file).ok().filter(std::fs::Metadata::is_file).ok_or(refuse(Status::NotFound, "Artifact not found"))?;
        if meta.len() > self.max_bundle_bytes as u64 {
            return Err(refuse(Status::ServerError, "Artifact is too large"));
        }
        let data = std::fs::read(&file).map_err(|_| refuse(Status::ServerError, "Remote error"))?;
        let mut metadata = Vec::new();
        let named = Value::Map(vec![(text("name"), Value::Binary(artifact.into_bytes()))]);
        rmpv::encode::write_value(&mut metadata, &named).map_err(|_| refuse(Status::ServerError, "Remote error"))?;
        Ok(Response::File { data, metadata: Some(metadata) })
    }

    fn release_init(
        &self,
        releases: &std::path::Path,
        repo_dir: &std::path::Path,
        map: &MapRef,
        peer: &[u8; 16],
    ) -> Result<Response, Refusal> {
        let tag = map.str("tag").filter(|t| plain_tag(t)).ok_or(refuse(Status::BadRequest, "Invalid tag name"))?;
        let repo = gix::open(repo_dir).map_err(|_| refuse(Status::ServerError, "Repository is unreadable"))?;
        if repo.find_reference(format!("refs/tags/{tag}").as_str()).is_err() {
            return Err(refuse(Status::BadRequest, &format!("Tag '{tag}' does not exist in repository")));
        }
        let notes = map.str("notes").unwrap_or_default();
        if notes.len() > MAX_NOTES_BYTES {
            return Err(refuse(Status::BadRequest, "Release notes are too long"));
        }
        let hash = map.str("hash").filter(|h| h.len() <= 64 && h.chars().all(|c| c.is_ascii_hexdigit()));
        let failed = || refuse(Status::ServerError, "Remote error");
        if std::fs::read_dir(releases).map_or(0, |entries| entries.count()) >= MAX_RELEASES {
            return Err(refuse(Status::ServerError, "Too many releases"));
        }
        std::fs::create_dir_all(releases).map_err(|_| failed())?;
        let dir = releases.join(&tag);
        // The folder is the claim: whoever makes it first has the tag.
        if let Err(e) = std::fs::create_dir(&dir) {
            return Err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                refuse(Status::NotAllowed, "Release already exists")
            } else {
                failed()
            });
        }
        let made = (|| {
            std::fs::create_dir(dir.join("artifacts"))?;
            let mut meta = vec![("tag".to_string(), tag.clone())];
            meta.extend(hash.map(|h| ("hash".to_string(), h)));
            meta.extend([
                ("created".to_string(), now().to_string()),
                ("status".to_string(), "draft".to_string()),
                ("created_by".to_string(), hex::encode(peer)),
            ]);
            write_meta(&dir, &meta)?;
            if !notes.is_empty() {
                let file = if map.str("notes_format").as_deref() == Some("micron") { "RELEASE.mu" } else { "RELEASE.md" };
                write_file(&dir.join(file), notes.as_bytes())?;
            }
            write_file(&dir.join("THANKS"), &[0x81, 0xa5, b'c', b'o', b'u', b'n', b't', 0x00])
        })();
        if made.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(failed());
        }
        Ok(ack())
    }

    /// The folder of a draft release, for adding to it.
    fn draft(releases: &std::path::Path, map: &MapRef) -> Result<std::path::PathBuf, Refusal> {
        let tag = map.str("tag").filter(|t| plain_tag(t)).ok_or(refuse(Status::BadRequest, "Invalid tag specified"))?;
        let dir = releases.join(tag);
        let meta = dir.is_dir().then(|| read_meta(&dir)).flatten().ok_or(refuse(Status::NotFound, "Release not found"))?;
        if meta_get(&meta, "status") != Some("draft") {
            return Err(refuse(Status::NotAllowed, "Release was finalized and is not writable"));
        }
        Ok(dir)
    }

    fn release_artifact(&self, releases: &std::path::Path, map: &MapRef) -> Result<Response, Refusal> {
        let name =
            map.str("artifact_name").filter(|n| plain_artifact(n)).ok_or(refuse(Status::BadRequest, "Missing tag or artifact name"))?;
        let data = match map.get("artifact_data") {
            Some(Value::Binary(bytes)) => bytes.as_slice(),
            Some(Value::String(s)) => s.as_bytes(),
            _ => return Err(refuse(Status::BadRequest, "No artifact data")),
        };
        let dir = Self::draft(releases, map)?;
        let folder = dir.join("artifacts");
        if data.len() > self.quotas.max_artifact_bytes {
            return Err(refuse(Status::BadRequest, "Artifact is too large"));
        }
        let kept = artifacts(&dir);
        let replaced = kept.iter().find(|(n, _)| *n == name).map_or(0, |(_, size)| *size);
        let total: u64 = kept.iter().map(|(_, size)| size).sum::<u64>() - replaced + data.len() as u64;
        if total > self.quotas.max_release_bytes || (replaced == 0 && kept.len() >= 1000) {
            return Err(refuse(Status::BadRequest, "Release is too large"));
        }
        if dir_size(releases).saturating_sub(replaced).saturating_add(data.len() as u64) > self.quotas.max_repo_bytes {
            return Err(refuse(Status::ServerError, "Releases have no room left"));
        }
        write_file(&folder.join(&name), data).map_err(|_| refuse(Status::ServerError, "Remote error"))?;
        Ok(ack())
    }

    fn release_finalize(&self, releases: &std::path::Path, map: &MapRef) -> Result<Response, Refusal> {
        let dir = Self::draft(releases, map)?;
        let tag = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let failed = || refuse(Status::ServerError, "Remote error");
        let mut meta = read_meta(&dir).ok_or_else(failed)?;
        meta.retain(|(k, _)| k != "status" && k != "published_at");
        meta.push(("status".into(), "published".into()));
        meta.push(("published_at".into(), now().to_string()));
        write_meta(&dir, &meta).map_err(|_| failed())?;
        // The newest published release is the latest.
        write_file(&releases.join("latest"), tag.as_bytes()).map_err(|_| failed())?;
        Ok(ack())
    }
}

fn release_delete(releases: &std::path::Path, map: &MapRef) -> Result<Response, Refusal> {
    let tag = map.str("tag").filter(|t| plain_tag(t)).ok_or(refuse(Status::BadRequest, "Invalid tag specified"))?;
    let dir = releases.join(tag);
    if !dir.is_dir() {
        return Err(refuse(Status::NotFound, "Release not found"));
    }
    std::fs::remove_dir_all(&dir).map_err(|_| refuse(Status::ServerError, "Remote error"))?;
    Ok(ack())
}

fn release_latest(releases: &std::path::Path, map: &MapRef) -> Result<Response, Refusal> {
    let tag = map.str("tag").filter(|t| plain_tag(t)).ok_or(refuse(Status::BadRequest, "Invalid tag specified"))?;
    if !releases.join(&tag).is_dir() {
        return Err(refuse(Status::NotFound, "Release not found"));
    }
    write_file(&releases.join("latest"), tag.as_bytes()).map_err(|_| refuse(Status::ServerError, "Remote error"))?;
    Ok(ack())
}
