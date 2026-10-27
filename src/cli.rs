//! The `rngit` command line: every client call as a subcommand.
//!
//! [`run`] takes the arguments after the program name and writes to `out`, so
//! it can be driven in tests without a process or a network.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::client::{Client, WorkKind, WorkScope};
use crate::error::{Error, Result};
use crate::sign::Signer;
use crate::transport::Transport;
use crate::url::RepoUrl;

pub const USAGE: &str = "\
usage: rngit <command> [arguments]

Repositories (URL is rns://<destination>/<group>/<repo>):
  refs URL                      list branches and tags
  clone URL DIR                 clone into a new bare repository
  create URL                    create an empty repository
  fork URL SOURCE               fork SOURCE (an rns, https or ssh URL) on the node
  mirror URL SOURCE             mirror SOURCE on the node
  push-all URL DIR [force] [prune]   push every branch and tag of the repository in DIR
  delete-ref URL REF            delete a branch or tag on the node (REF is refs/heads/NAME or refs/tags/NAME)
  sync URL                      update a fork or mirror from its source
A copy on this computer (DIR is a bare repository, REF is refs/heads/NAME or refs/tags/NAME):
  log DIR REF [COUNT]           the newest commits
  tree DIR REF [PATH]           the files in a folder
  show DIR REF PATH             a file's text
Permissions (TEXT is the new file content, or - to read standard input):
  perms URL [TEXT]              show or replace a repository's permissions
  gperms URL [TEXT]             show or replace the permissions of URL's group
Releases:
  releases URL                  list releases
  release URL TAG               show one release
  release-get URL TAG NAME DIR  download one artifact, checked against the node's hash
  release-new URL TAG NOTES FILE...   publish a signed release (the tag must be pushed)
  release-delete URL TAG
  release-latest URL TAG        make TAG the latest release
  verify MANIFEST [DIR]         check a downloaded manifest and the files in DIR (default: its folder), offline
Work documents (SCOPE is active, completed, proposed or all):
  work URL [SCOPE]              list documents
  work-view URL ID [SCOPE]      show one document and check its signature
  work-new URL TITLE TEXT       create a document (work-propose to propose one)
  work-comment URL ID TEXT
  work-edit URL ID TITLE TEXT
  work-complete URL ID    work-activate URL ID    work-delete URL ID [SCOPE]
Network:
  nodes [SECONDS]               listen for rngit nodes (the rngit program handles this)
";

fn scope(arg: Option<&String>) -> Result<WorkScope> {
    Ok(match arg.map(String::as_str) {
        None | Some("active") => WorkScope::Active,
        Some("completed") => WorkScope::Completed,
        Some("proposed") => WorkScope::Proposed,
        Some("all") => WorkScope::All,
        Some(other) => return Err(Error::InvalidInput(format!("unknown scope {other:?}"))),
    })
}

fn id(arg: &str) -> Result<u64> {
    arg.parse().map_err(|_| Error::InvalidInput(format!("{arg:?} is not a document id")))
}

/// Text from the command line, or from `input` when it is `-`.
fn text(arg: &str, input: &mut dyn Read) -> Result<String> {
    if arg != "-" {
        return Ok(arg.to_string());
    }
    let mut text = String::new();
    input.take(1024 * 1024).read_to_string(&mut text).map_err(Error::git)?;
    Ok(text)
}

fn say(out: &mut dyn Write, line: impl AsRef<str>) -> Result<()> {
    writeln!(out, "{}", line.as_ref()).map_err(Error::git)
}

/// The commit a tag in the repository around the current folder points at.
fn commit_of_tag(tag: &str) -> Option<String> {
    let repo = gix::discover(".").ok()?;
    let mut reference = repo.find_reference(format!("refs/tags/{tag}").as_str()).ok()?;
    Some(reference.peel_to_commit().ok()?.id.to_string())
}

/// Whether `command` works on files here and needs no node or URL.
pub fn is_local(command: &str) -> bool {
    matches!(command, "log" | "tree" | "show" | "verify")
}

/// The commands that read a copy on this computer, which need no node.
pub fn local_command(command: &str, rest: &[String], out: &mut dyn Write) -> Result<()> {
    let arg =
        |n: usize, what: &str| rest.get(n).map(String::as_str).ok_or_else(|| Error::InvalidInput(format!("{command}: missing {what}")));
    if command == "verify" {
        return verify_command(Path::new(arg(0, "MANIFEST")?), rest.get(1).map(Path::new), out);
    }
    let (dir, refname) = (Path::new(arg(0, "DIR")?), arg(1, "REF")?);
    match command {
        "log" => {
            let count = match rest.get(2) {
                Some(n) => n.parse().map_err(|_| Error::InvalidInput(format!("{n:?} is not a count")))?,
                None => 20,
            };
            for commit in crate::local::log(dir, refname, count)? {
                say(out, format!("{} {} {} {}", commit.id, iso8601(commit.time.max(0) as u64), commit.author, commit.subject))?;
            }
        }
        "tree" => {
            for entry in crate::local::tree(dir, refname, rest.get(2).map_or("", String::as_str))? {
                let mark = match entry.kind {
                    crate::local::EntryKind::Dir => "/",
                    crate::local::EntryKind::Link => "@",
                    crate::local::EntryKind::Submodule => "#",
                    crate::local::EntryKind::File => "",
                };
                say(out, format!("{}{mark}{}", entry.name, entry.size.map_or(String::new(), |size| format!("  {size}"))))?;
            }
        }
        _ => {
            let view = crate::local::file(dir, refname, arg(2, "PATH")?, 1024 * 1024)?;
            match view.text {
                Some(text) => write!(out, "{text}").map_err(Error::git)?,
                None => say(out, format!("binary file, {} bytes", view.size))?,
            }
        }
    }
    Ok(())
}

/// Largest file `verify` reads into memory.
const MAX_VERIFY_FILE: u64 = 1 << 30;

/// Check a downloaded release with no node: the manifest's signature, then
/// each file it lists that is in `dir`, against that file's signature.
fn verify_command(manifest_path: &Path, dir: Option<&Path>, out: &mut dyn Write) -> Result<()> {
    let read = |path: &Path| -> Result<Vec<u8>> {
        let size = std::fs::metadata(path).map_err(Error::git)?.len();
        if size > MAX_VERIFY_FILE {
            return Err(Error::TooLarge { what: "file", size: size as usize, limit: MAX_VERIFY_FILE as usize });
        }
        std::fs::read(path).map_err(Error::git)
    };
    let manifest = crate::sign::verify_manifest(&read(manifest_path)?)?;
    say(
        out,
        format!("Valid release manifest for {} {}, signed by {}", manifest.name, manifest.version, hex::encode(manifest.signer.signer)),
    )?;
    let dir = dir.or_else(|| manifest_path.parent()).filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let (mut valid, mut checked) = (0, 0);
    for artifact in &manifest.artifacts {
        checked += 1;
        // A name from the manifest is a file in the folder and nothing else.
        let name = Path::new(&artifact.name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let path = dir.join(&name);
        if name.is_empty() || name != artifact.name || !path.is_file() {
            say(out, format!("  File {} from the manifest does not exist here, cannot validate", artifact.name))?;
            continue;
        }
        match crate::sign::verify_rsg(&read(&path)?, &artifact.rsg) {
            Ok(info) if info.signer == manifest.signer.signer => {
                valid += 1;
                say(out, format!("  File {name} validated against the manifest"))?;
            }
            Ok(_) => say(out, format!("  File {name} was signed by someone else than the release"))?,
            Err(_) => say(out, format!("  File {name} does not match the manifest"))?,
        }
    }
    if checked == 0 || valid != checked {
        say(out, "\nRelease is not valid")?;
        return Err(Error::Verify("the release is not valid".into()));
    }
    say(out, "\nAll files validated")
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso8601(secs: u64) -> String {
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil-from-days (proleptic Gregorian).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// Run one command. `now` is the current Unix time.
pub async fn run<T: Transport>(
    client: &Client<T>,
    signer: &dyn Signer,
    now: u64,
    args: &[String],
    input: &mut dyn Read,
    out: &mut dyn Write,
) -> Result<()> {
    let Some((command, rest)) = args.split_first() else { return say(out, USAGE.trim_end()) };
    let arg =
        |n: usize, what: &str| rest.get(n).map(String::as_str).ok_or_else(|| Error::InvalidInput(format!("{command}: missing {what}")));
    if matches!(command.as_str(), "help" | "-h" | "--help") {
        return say(out, USAGE.trim_end());
    }
    if is_local(command) {
        return local_command(command, rest, out);
    }
    let repo: RepoUrl = arg(0, "URL")?.parse()?;
    match command.as_str() {
        "refs" => {
            let refs = client.list(&repo, false).await?;
            for (sha, name) in &refs.refs {
                say(out, format!("{sha} {name}"))?;
            }
            if let Some(head) = &refs.head {
                say(out, format!("HEAD -> {head}"))?;
            }
        }
        "clone" => {
            let dir = PathBuf::from(arg(1, "DIR")?);
            if dir.exists() && dir.read_dir().map_err(Error::git)?.next().is_some() {
                return Err(Error::InvalidInput(format!("{} is not empty", dir.display())));
            }
            let refs = client.clone_into(&repo, &dir).await?;
            say(out, format!("cloned {} refs into {}", refs.refs.len(), dir.display()))?;
        }
        "push-all" => {
            let dir = PathBuf::from(arg(1, "DIR")?);
            let flag = |name: &str| rest[2..].iter().any(|a| a == name);
            if let Some(other) = rest[2..].iter().find(|a| !matches!(a.as_str(), "force" | "prune")) {
                return Err(Error::InvalidInput(format!("push-all: unknown option {other:?}")));
            }
            let report = client.push_all(&repo, &dir, flag("force"), flag("prune")).await?;
            say(
                out,
                format!(
                    "pushed {}, up to date {}, rejected {}, deleted {}",
                    report.pushed.len(),
                    report.up_to_date.len(),
                    report.rejected.len(),
                    report.deleted.len()
                ),
            )?;
            for name in &report.rejected {
                say(out, format!("rejected (not a fast-forward): {name}"))?;
            }
        }
        "create" => client.create(&repo).await?,
        "fork" => client.fork(&repo, arg(1, "SOURCE")?).await?,
        "mirror" => client.mirror(&repo, arg(1, "SOURCE")?).await?,
        "sync" => client.sync(&repo).await?,
        "delete-ref" => client.delete_ref(&repo, arg(1, "REF")?).await?,
        "perms" | "gperms" => {
            let group = command == "gperms";
            match rest.get(1) {
                None => say(out, if group { client.group_perms(&repo.group).await? } else { client.repo_perms(&repo).await? })?,
                Some(content) => {
                    let content = text(content, input)?;
                    if group {
                        client.set_group_perms(&repo.group, &content).await?
                    } else {
                        client.set_repo_perms(&repo, &content).await?
                    }
                }
            }
        }
        "releases" => {
            let list = client.releases(&repo).await?;
            for r in &list.releases {
                let latest = if list.latest.as_deref() == Some(r.tag.as_str()) { " (latest)" } else { "" };
                say(out, format!("{}{latest}  {}  {} artifacts  {}", r.tag, r.status, r.artifacts, r.preview))?;
            }
        }
        "release" => {
            let r = client.release(&repo, arg(1, "TAG")?).await?;
            say(out, format!("{}  {}  created {}  by {}", r.tag, r.status, iso8601(r.created), r.created_by))?;
            say(out, r.notes)?;
            for a in &r.artifacts {
                say(out, format!("  {}  {} bytes", a.name, a.size))?;
            }
        }
        "release-get" => {
            let (tag, name, dir) = (arg(1, "TAG")?, arg(2, "NAME")?, Path::new(arg(3, "DIR")?));
            // The signed manifest can be saved too, to check the files offline later with `verify`.
            let data = if name == "manifest.rsm" {
                let rsm = client.fetch_artifact(&repo, tag, name).await?;
                crate::sign::verify_manifest(&rsm)?;
                rsm
            } else {
                let (_, files) = client.fetch_verified_release(&repo, tag).await?;
                files
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, data)| data)
                    .ok_or_else(|| Error::InvalidInput(format!("release has no artifact {name:?}")))?
            };
            crate::client::check_artifact_name(name)?;
            std::fs::create_dir_all(dir).map_err(Error::git)?;
            std::fs::write(dir.join(name), &data).map_err(Error::git)?;
            say(out, format!("saved {} ({} bytes), signature and hash verified", dir.join(name).display(), data.len()))?;
        }
        "release-new" => {
            let (tag, notes) = (arg(1, "TAG")?, text(arg(2, "NOTES")?, input)?);
            let mut files = Vec::new();
            for path in rest.iter().skip(3) {
                let name = Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| Error::InvalidInput(format!("bad file {path:?}")))?;
                files.push((name.to_string(), std::fs::read(path).map_err(Error::git)?));
            }
            // Like stock, take the commit from the repository in the current folder, if it has the tag.
            let commit = commit_of_tag(tag);
            if commit.is_none() {
                say(out, format!("could not get the commit for tag {tag}; is the tag in the repository in this folder?"))?;
            }
            client.create_release(&repo, signer, tag, commit.as_deref(), &notes, &files, &iso8601(now), now).await?;
        }
        "release-delete" => client.delete_release(&repo, arg(1, "TAG")?).await?,
        "release-latest" => client.set_latest_release(&repo, arg(1, "TAG")?).await?,
        "work" => {
            let list = client.work_list(&repo, scope(rest.get(1))?).await?;
            for (label, docs) in [("active", &list.active), ("completed", &list.completed), ("proposed", &list.proposed)] {
                for d in docs {
                    say(out, format!("{label}  #{}  {}  ({} comments)", d.id, d.title, d.comments))?;
                }
            }
        }
        "work-view" => {
            let doc = client.work_view(&repo, id(arg(1, "ID")?)?, scope(rest.get(2))?).await?;
            let check = if doc.verified { "signature verified" } else { "SIGNATURE NOT VERIFIED" };
            say(out, format!("#{} {} [{}] by {}  {check}", doc.id, doc.title, doc.scope, doc.author))?;
            say(out, doc.content)?;
            say(out, format!("{} comments", doc.comments.len()))?;
        }
        "work-new" | "work-propose" => {
            let kind = if command == "work-new" { WorkKind::Create } else { WorkKind::Propose };
            let (id, scope) = client.work_create(&repo, signer, kind, arg(1, "TITLE")?, &text(arg(2, "TEXT")?, input)?, "markdown").await?;
            say(out, format!("created #{id} in {scope}"))?;
        }
        "work-comment" => {
            let comment =
                client.work_comment(&repo, id(arg(1, "ID")?)?, WorkScope::Active, &text(arg(2, "TEXT")?, input)?, "markdown").await?;
            say(out, format!("comment {comment}"))?;
        }
        "work-edit" => {
            let (doc, title) = (id(arg(1, "ID")?)?, arg(2, "TITLE")?);
            client.work_edit(&repo, signer, doc, WorkScope::Active, title, &text(arg(3, "TEXT")?, input)?).await?
        }
        "work-complete" | "work-activate" => {
            let doc = id(arg(1, "ID")?)?;
            let (id, scope) =
                if command == "work-complete" { client.work_complete(&repo, doc).await? } else { client.work_activate(&repo, doc).await? };
            say(out, format!("#{id} is now {scope}"))?;
        }
        "work-delete" => client.work_delete(&repo, id(arg(1, "ID")?)?, scope(rest.get(2))?).await?,
        other => return Err(Error::InvalidInput(format!("unknown command {other:?}; try `rngit help`"))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_utc_iso() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601(1_793_033_000), "2026-10-26T16:43:20Z");
        assert_eq!(iso8601(4_102_444_799), "2099-12-31T23:59:59Z");
    }
}
