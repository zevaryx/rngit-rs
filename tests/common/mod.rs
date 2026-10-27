//! An in-process stand-in for an rngit node. It speaks the same request and
//! reply formats as the spec, backed by a real bare repository.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use rmpv::Value;
use rngit::bundle;
use rngit::error::Error;
use rngit::wire::MapRef;
use rngit::{Response, Transport};

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A working repository with `n` commits on `main`.
pub fn work_repo(dir: &Path, n: usize) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    for i in 0..n {
        std::fs::write(
            dir.join(format!("file{i}.txt")),
            format!("{} content {i}\n", dir.file_name().unwrap().to_string_lossy()).repeat(50),
        )
        .unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-q", "-m", &format!("commit {i}")]);
    }
}

pub fn bare_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "--bare", "-b", "main"]);
}

pub type Handler = Box<dyn Fn(&str, &Value) -> Option<Response> + Send + Sync>;

/// A node with one bare repository, `public/demo.git`, plus an optional
/// handler for anything else (releases, work documents, permissions).
pub struct MockNode {
    pub repo: PathBuf,
    pub allow_write: bool,
    pub extra: Handler,
    pub log: Mutex<Vec<(String, Value)>>,
}

impl MockNode {
    pub fn new(repo: PathBuf) -> Self {
        MockNode { repo, allow_write: true, extra: Box::new(|_, _| None), log: Mutex::new(Vec::new()) }
    }

    pub fn requests(&self) -> Vec<(String, Value)> {
        self.log.lock().unwrap().clone()
    }
}

pub fn ok(payload: &[u8]) -> Response {
    let mut reply = vec![0u8];
    reply.extend_from_slice(payload);
    Response::Packet(reply)
}

pub fn status(byte: u8, text: &str) -> Response {
    let mut reply = vec![byte];
    reply.extend_from_slice(text.as_bytes());
    Response::Packet(reply)
}

pub fn pack(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, value).unwrap();
    out
}

pub fn field<'a>(request: &'a Value, key: &str) -> Option<&'a Value> {
    MapRef::new(request).ok()?.get(key)
}

impl Transport for MockNode {
    async fn request(&self, path: &str, data: Vec<u8>) -> Result<Response, Error> {
        let request = rmpv::decode::read_value(&mut data.as_slice()).expect("client sent valid MessagePack");
        self.log.lock().unwrap().push((path.to_string(), request.clone()));
        if let Some(response) = (self.extra)(path, &request) {
            return Ok(response);
        }
        let Value::Map(entries) = &request else { return Ok(status(2, "Invalid request")) };
        let repo_name = entries.iter().find(|(k, _)| k.as_u64() == Some(0)).and_then(|(_, v)| v.as_str());
        if repo_name != Some("public/demo.git") && path.starts_with("/git/") && path != "/git/create" {
            return Ok(status(3, "Not found"));
        }
        let repo = gix::open(&self.repo).unwrap();
        Ok(match path {
            "/git/list" => {
                let mut text = String::new();
                for r in repo.references().unwrap().all().unwrap() {
                    let r = r.unwrap();
                    text.push_str(&format!("{} {}\n", r.id(), r.name().as_bstr()));
                }
                let head = repo.find_reference("HEAD").unwrap();
                text.push_str(&format!("@{} HEAD\n", head.target().try_name().map(|n| n.as_bstr().to_string()).unwrap_or_default()));
                ok(text.as_bytes())
            }
            "/git/fetch" => {
                let refs = field(&request, "refs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                if refs.is_empty() {
                    return Ok(status(2, "No refs specified"));
                }
                let name = MapRef::new(&refs[0]).unwrap().str("ref").unwrap();
                // Like the real node, leave out history the client says it has.
                let have =
                    MapRef::new(&refs[0]).unwrap().str("have").and_then(|h| gix::ObjectId::from_hex(h.as_bytes()).ok()).or_else(|| {
                        let tips = field(&request, "have")?.as_array()?.clone();
                        tips.iter()
                            .filter_map(|v| gix::ObjectId::from_hex(v.as_str()?.as_bytes()).ok())
                            .find(|id| repo.find_object(*id).is_ok())
                    });
                // A real node answers with one bundle holding every ref asked for.
                let data = if refs.len() > 1 {
                    let mut args = vec!["bundle".to_string(), "create".into(), "-".into()];
                    args.extend(refs.iter().map(|r| MapRef::new(r).unwrap().str("ref").unwrap()));
                    args.extend(have.map(|id| format!("^{id}")));
                    let out = std::process::Command::new("git").current_dir(repo.git_dir()).args(&args).output().unwrap();
                    if out.status.success() {
                        out.stdout
                    } else {
                        // Git won't make a bundle of nothing; a node sends the header alone.
                        let mut data = b"# v2 git bundle\n".to_vec();
                        data.extend(have.map(|id| format!("-{id} have\n")).unwrap_or_default().into_bytes());
                        for r in &refs {
                            let name = MapRef::new(r).unwrap().str("ref").unwrap();
                            let id = repo.find_reference(name.as_str()).unwrap().id();
                            data.extend(format!("{id} {name}\n").into_bytes());
                        }
                        data.push(b'\n');
                        data
                    }
                } else {
                    bundle::build(&repo, &name, have).unwrap()
                };
                Response::File { data, metadata: Some(vec![0x81, 0x01, 0x00]) }
            }
            "/git/delete" => {
                let name = field(&request, "ref").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if !name.starts_with("refs/") {
                    return Ok(status(2, "Invalid request"));
                }
                if let Ok(reference) = repo.find_reference(name.as_str()) {
                    reference.delete().unwrap();
                }
                ok(b"")
            }
            "/git/push" => {
                if let Some(ops) = field(&request, "operations").and_then(|v| v.as_array()) {
                    let op = MapRef::new(&ops[0]).unwrap();
                    let (name, sha) = (op.str("ref").unwrap(), op.str("sha").unwrap());
                    let Ok(id) = gix::ObjectId::from_hex(sha.as_bytes()) else { return Ok(status(2, "Invalid request data")) };
                    if repo.find_object(id).is_err() {
                        return Ok(status(0xff, &format!("Object {sha} does not exist in repository")));
                    }
                    repo.reference(name.as_str(), id, gix::refs::transaction::PreviousValue::Any, "test").unwrap();
                    return Ok(ok(b""));
                }
                if !self.allow_write {
                    return Ok(status(1, "Not allowed"));
                }
                let force = field(&request, "force").and_then(|v| v.as_bool()).unwrap_or(false);
                let remote_ref = field(&request, "remote_ref").and_then(|v| v.as_str()).unwrap().to_string();
                let data = match field(&request, "bundle") {
                    Some(Value::Binary(b)) => b.clone(),
                    _ => return Ok(status(2, "Invalid request data")),
                };
                let Ok(header) = bundle::parse_header(&data) else { return Ok(status(0xff, "Could not verify bundle")) };
                // A prerequisite that isn't the current tip is a diverged push.
                if let Some(pre) = header.prerequisites.first() {
                    let tip = repo.find_reference(remote_ref.as_str()).ok().map(|mut r| r.peel_to_id().unwrap().detach());
                    if !force && tip != Some(*pre) {
                        return Ok(status(0xff, "Could not verify bundle"));
                    }
                }
                match bundle::ingest(&repo, &data) {
                    Ok(refs) => {
                        // Ingest sets the bundle's own ref name; the node maps it to remote_ref.
                        if let Some((id, name)) = refs.first()
                            && *name != remote_ref
                        {
                            repo.reference(remote_ref.as_str(), *id, gix::refs::transaction::PreviousValue::Any, "push").unwrap();
                        }
                        ok(b"")
                    }
                    Err(_) => status(0xff, "Could not verify bundle"),
                }
            }
            "/git/create" => ok(b""),
            "/git/fork" | "/git/mirror" => match field(&request, "source") {
                Some(_) => ok(b""),
                None => status(2, "No source specified"),
            },
            "/git/sync" => ok(b""),
            _ => return Err(Error::Transport(format!("unknown request path {path}"))),
        })
    }
}
