//! The git remote helper protocol (`gitremote-helpers`) on top of [`Client`].
//!
//! Git starts `git-remote-rns <remote> <url>` and talks to it over a pipe.
//! [`run`] is the whole conversation, so it can be tested without a process.

use std::io::{BufRead, Read, Write};
use std::path::Path;

use gix::ObjectId;

use crate::client::{Client, RemoteRefs, Want};
use crate::error::{Error, Result};
use crate::transport::Transport;
use crate::url::RepoUrl;

/// Longest command line git is allowed to send us.
const MAX_LINE: u64 = 64 * 1024;
/// Most refs one fetch or push batch may name.
const MAX_BATCH: usize = 10_000;

fn read_line(input: &mut dyn BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    let mut reader = &mut *input;
    let n = Read::take(&mut reader, MAX_LINE).read_line(&mut line).map_err(Error::git)?;
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') && n as u64 >= MAX_LINE {
        return Err(Error::Protocol("helper command line is too long".into()));
    }
    Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
}

/// Read command lines until the blank line that ends a batch.
fn read_batch(input: &mut dyn BufRead, first: String, prefix: &str) -> Result<Vec<String>> {
    let mut items = vec![first];
    while let Some(line) = read_line(input)? {
        if line.is_empty() {
            break;
        }
        let rest = line.strip_prefix(prefix).ok_or_else(|| Error::Protocol(format!("unexpected line in a batch: {line:?}")))?;
        items.push(rest.to_string());
        if items.len() > MAX_BATCH {
            return Err(Error::Protocol("batch has too many refs".into()));
        }
    }
    Ok(items)
}

fn no_git_dir() -> Error {
    Error::InvalidInput("GIT_DIR is not set; run this through git".into())
}

fn out(output: &mut dyn Write, text: &str) -> Result<()> {
    output.write_all(text.as_bytes()).map_err(Error::git)
}

/// Serve one helper session. `git_dir` is the local repository that fetches
/// write into and pushes read from; without it (as in `git ls-remote`) only
/// `list` works. Returns when git closes the pipe.
pub async fn run<T: Transport>(
    client: &Client<T>,
    repo: &RepoUrl,
    git_dir: Option<&Path>,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<()> {
    let mut known: Option<RemoteRefs> = None;
    let mut known_for_push: Option<RemoteRefs> = None;
    while let Some(line) = read_line(input)? {
        let (command, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        match command {
            "" => {}
            "capabilities" => out(output, "fetch\npush\noption\n\n")?,
            "option" => out(output, "unsupported\n")?,
            "list" => {
                let refs = client.list(repo, rest == "for-push").await?;
                let mut text = String::new();
                for (sha, name) in &refs.refs {
                    text.push_str(&format!("{sha} {name}\n"));
                }
                if let Some(head) = &refs.head {
                    text.push_str(&format!("@{head} HEAD\n"));
                }
                text.push('\n');
                out(output, &text)?;
                if rest == "for-push" {
                    known_for_push = Some(refs.clone());
                }
                known = Some(refs);
            }
            "fetch" => {
                let mut wants = Vec::new();
                for item in read_batch(input, rest.to_string(), "fetch ")? {
                    let (sha, name) = item.split_once(' ').ok_or_else(|| Error::Protocol(format!("bad fetch line {item:?}")))?;
                    let sha = ObjectId::from_hex(sha.as_bytes()).map_err(|_| Error::Protocol(format!("bad object id {sha:?}")))?;
                    // Git can ask for HEAD by name; the node knows it by its branch.
                    let name = match (name, known.as_ref().and_then(|r| r.head.as_ref())) {
                        ("HEAD", Some(head)) => head.clone(),
                        _ => name.to_string(),
                    };
                    if !wants.iter().any(|w: &Want| w.name == name) {
                        wants.push(Want { sha, name });
                    }
                }
                client.fetch_objects(repo, &wants, git_dir.ok_or_else(no_git_dir)?).await?;
                out(output, "\n")?;
            }
            "push" => {
                let mut text = String::new();
                for item in read_batch(input, rest.to_string(), "push ")? {
                    let (force, spec) = item.strip_prefix('+').map_or((false, item.as_str()), |s| (true, s));
                    let Some((src, dst)) = spec.split_once(':') else {
                        return Err(Error::Protocol(format!("bad push line {item:?}")));
                    };
                    let result = if src.is_empty() {
                        client.delete_ref(repo, dst).await
                    } else {
                        let git_dir = git_dir.ok_or_else(no_git_dir)?;
                        // Ask the node for its refs once per push, as stock rngit does.
                        if known_for_push.is_none() {
                            known_for_push = Some(client.list(repo, true).await?);
                        }
                        let remote = known_for_push.as_ref().expect("just set");
                        client.push_from_with(repo, git_dir, src, dst, force, remote).await
                    };
                    match result {
                        Ok(()) => text.push_str(&format!("ok {dst}\n")),
                        // The reason is one line: git prints it after the ref name.
                        Err(e) => text.push_str(&format!("error {dst} {}\n", e.to_string().replace('\n', " "))),
                    }
                }
                text.push('\n');
                out(output, &text)?;
            }
            other => return Err(Error::Protocol(format!("unknown helper command {other:?}"))),
        }
        output.flush().map_err(Error::git)?;
    }
    Ok(())
}
