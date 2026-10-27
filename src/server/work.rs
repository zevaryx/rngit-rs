//! Work documents on a node: `/mgmt/work`.
//!
//! The documents of `group/name.git` live in `group/name.git.work/`, as stock
//! rngit keeps them, so a folder can be served by either:
//!
//! - `active/<id>/`, `completed/<id>/` and `proposed/<id>/`: a document.
//!   `root` is the document, a MessagePack map of its `content` and its
//!   `meta` (title, format, times, the author's hash, the author's public
//!   key and the signature over the content); the files named `1`, `2`, ...
//!   are its comments, in the same form;
//! - `<id>.allowed`: who may do what with that document, in the form of a
//!   repository's. Proposing a document makes one that gives its author
//!   `i` and `w`.
//!
//! A document's content is signed by its author, and the node checks the
//! signature against the public key the author's link proved before it keeps
//! anything. Readers check it again.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use rmpv::Value;

use super::support::{now_f64, packed, refuse, text, write_file};
use super::{Node, Perm, Refusal, access, ack, dir_size, int_key, read_rules, split_repo_path};
use crate::error::Status;
use crate::sign;
use crate::transport::Response;
use crate::wire::MapRef;

/// The scopes a document is in, where each is.
const SCOPES: [&str; 3] = ["active", "completed", "proposed"];
/// Most the title, text and format of a document may come to, in characters.
const MAX_DOC_CHARS: usize = 256 * 1024;
/// Largest document or comment file read.
const MAX_DOC_FILE: u64 = 2 * 1024 * 1024;
/// Documents one repository keeps.
const MAX_DOCUMENTS: usize = 5000;
/// Comments one document keeps.
const MAX_COMMENTS: usize = 1000;

/// A document or a comment, as it is kept.
struct Doc {
    content: String,
    title: Option<String>,
    format: String,
    created: f64,
    edited: f64,
    author: Vec<u8>,
    signature: Option<Vec<u8>>,
    identity: Option<Vec<u8>>,
}

impl Doc {
    fn read(path: &Path) -> Option<Doc> {
        let mut bytes = Vec::new();
        std::fs::File::open(path).ok()?.take(MAX_DOC_FILE).read_to_end(&mut bytes).ok()?;
        let value = crate::wire::decode(&bytes).ok()?;
        let doc = MapRef::new(&value).ok()?;
        let meta = doc.map("meta");
        let meta = meta.as_ref();
        Some(Doc {
            content: doc.str("content").unwrap_or_default(),
            title: meta.and_then(|m| m.str("title")),
            format: meta.and_then(|m| m.str("format")).unwrap_or_else(|| "markdown".into()),
            created: meta.and_then(|m| m.f64("created")).unwrap_or(0.0),
            edited: meta.and_then(|m| m.f64("edited")).unwrap_or(0.0),
            author: meta.and_then(|m| m.bytes("author")).map(<[u8]>::to_vec).unwrap_or_default(),
            signature: meta.and_then(|m| m.bytes("signature")).map(<[u8]>::to_vec),
            identity: meta.and_then(|m| m.bytes("identity")).map(<[u8]>::to_vec),
        })
    }

    fn encode(&self) -> Vec<u8> {
        let binary = |bytes: &Option<Vec<u8>>| bytes.clone().map_or(Value::Nil, Value::Binary);
        let mut meta = vec![
            (text("format"), text(&self.format)),
            (text("title"), self.title.as_deref().map_or(Value::Nil, text)),
            (text("created"), Value::from(self.created)),
            (text("edited"), Value::from(self.edited)),
            (text("author"), Value::Binary(self.author.clone())),
            (text("signature"), binary(&self.signature)),
        ];
        // A comment carries no key: the document's author's is what is checked.
        if self.identity.is_some() {
            meta.push((text("identity"), binary(&self.identity)));
        }
        let mut out = Vec::new();
        let doc = Value::Map(vec![(text("content"), text(&self.content)), (text("meta"), Value::Map(meta))]);
        // Writing to a Vec can't fail.
        let _ = rmpv::encode::write_value(&mut out, &doc);
        out
    }

    fn author_hex(&self) -> String {
        hex::encode(&self.author)
    }
}

fn format_of(map: &MapRef) -> String {
    match map.str("format").as_deref() {
        Some("micron") => "micron".into(),
        _ => "markdown".into(),
    }
}

/// A document's number, as a client may send it: an integer, or digits.
fn doc_number(value: &Value) -> Option<u64> {
    match value {
        Value::String(s) => s.as_str()?.trim().parse().ok(),
        other => other.as_u64(),
    }
}

/// Where a document is: its scope and its folder.
fn find_doc(work: &Path, id: u64) -> Option<(&'static str, PathBuf)> {
    SCOPES.iter().map(|scope| (*scope, work.join(scope).join(id.to_string()))).find(|(_, dir)| dir.is_dir())
}

/// The numbers of the documents (or comments) in a folder, by their names.
fn numbers(dir: &Path, folders: bool) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| if folders { t.is_dir() } else { t.is_file() }))
        .filter_map(|e| e.file_name().to_str().filter(|n| n.chars().all(|c| c.is_ascii_digit())).and_then(|n| n.parse().ok()))
        .collect()
}

/// The next number for a document: past every one in any scope.
fn next_id(work: &Path) -> u64 {
    SCOPES.iter().flat_map(|scope| numbers(&work.join(scope), true)).max().map_or(1, |m| m.saturating_add(1))
}

fn failed() -> Refusal {
    refuse(Status::ServerError, "Remote error")
}

impl Node {
    fn doc_grants(&self, work: &Path, id: u64) -> access::Grants {
        access::Grants::from_rules(&read_rules(&work.join(format!("{id}.allowed"))))
    }

    /// `/mgmt/work`, as stock rngit answers it: who may do what with a
    /// document (see [`access`]) decides each operation, and the signature of
    /// what is written is checked against `key`, the public key of the
    /// identity the peer proved.
    pub(super) fn work(&self, request: &Value, map: &MapRef, peer: &[u8; 16], key: Option<&[u8; 64]>) -> Result<Response, Refusal> {
        if *peer == super::NULL_IDENT {
            return Err(refuse(Status::NotAllowed, "Not identified"));
        }
        let name = int_key(request, 0).ok_or(refuse(Status::BadRequest, "No repository specified"))?;
        let operation = map.str("operation").ok_or(refuse(Status::BadRequest, "Invalid request"))?;
        let (group, repo) = split_repo_path(&name).ok_or(refuse(Status::NotFound, "Not found"))?;
        if !self.root.join(group).join(repo).is_dir() || !self.allowed(group, repo, peer, Perm::Read) {
            return Err(refuse(Status::NotFound, "Not found"));
        }
        let work = self.root.join(group).join(format!("{repo}.work"));
        let (repo_grants, group_grants) = (self.repo_grants(group, repo), self.group_grants(group));
        let has = |perm| access::repository(peer, perm, &repo_grants, &group_grants);
        let (mut read, mut write, mut interact) = (true, has(Perm::Write), has(Perm::Issues));
        let (propose, admin) = (has(Perm::Proposals), has(Perm::Admin));

        let doc_id = match map.get("doc_id") {
            Some(value) => Some(doc_number(value).ok_or(refuse(Status::BadRequest, "Invalid request"))?),
            None => None,
        };
        if let Some(id) = doc_id {
            let doc = self.doc_grants(&work, id);
            let on_doc = |perm| access::document(peer, perm, &doc, &repo_grants, &group_grants);
            if matches!(operation.as_str(), "read" | "view" | "comment" | "edit" | "delete" | "perms") {
                read = on_doc(Perm::Read) || admin;
                if !read {
                    return Err(refuse(Status::NotFound, "Document not found"));
                }
            }
            if matches!(operation.as_str(), "comment" | "edit") {
                interact |= on_doc(Perm::Issues);
            }
            if operation == "edit" {
                write |= on_doc(Perm::Write);
            }
        }
        let comment = interact && (read || write);
        let manage = interact && write;
        let allowed = match operation.as_str() {
            "list" | "view" => read,
            "comment" => comment,
            "propose" => propose,
            "create" | "edit" | "delete" | "complete" | "activate" => manage,
            "perms" => admin,
            _ => false,
        };
        if !allowed {
            return Err(refuse(Status::NotAllowed, "Not allowed"));
        }
        // Writing is one at a time, so two documents never get one number.
        let _writing = self.working.lock().map_err(|_| failed())?;
        let who = Who { me: peer, key, work: &work };
        match operation.as_str() {
            "list" => self.work_list(&work, map, peer, &repo_grants, &group_grants),
            "view" => work_view(&work, map, doc_id),
            "create" => self.work_new(&who, map, "active"),
            "propose" => self.work_new(&who, map, "proposed"),
            "comment" => self.work_comment(&who, map, doc_id),
            "edit" => work_edit(&who, map, doc_id),
            "delete" => self.work_delete(&who, doc_id, &repo_grants, &group_grants),
            "complete" => self.work_move(&who, doc_id, &["active"], "completed", &repo_grants, &group_grants),
            "activate" => self.work_move(&who, doc_id, &["completed", "proposed"], "active", &repo_grants, &group_grants),
            _ => self.work_perms(&who, map, doc_id, &repo_grants, &group_grants),
        }
    }

    fn work_list(
        &self,
        work: &Path,
        map: &MapRef,
        peer: &[u8; 16],
        repo: &access::Grants,
        group: &access::Grants,
    ) -> Result<Response, Refusal> {
        let scope = map.str("scope").unwrap_or_else(|| "active".into());
        let mut lists = Vec::new();
        for folder in SCOPES {
            let mut found = Vec::new();
            if scope == folder || scope == "all" {
                for id in numbers(&work.join(folder), true).into_iter().take(MAX_DOCUMENTS) {
                    if !access::document(peer, Perm::Read, &self.doc_grants(work, id), repo, group) {
                        continue;
                    }
                    let dir = work.join(folder).join(id.to_string());
                    let Some(doc) = Doc::read(&dir.join("root")) else { continue };
                    let comments = numbers(&dir, false).len() as u64;
                    found.push((doc.created, doc, id, comments));
                }
            }
            found.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.2.cmp(&b.2)));
            let items: Vec<Value> = found
                .into_iter()
                .map(|(_, doc, id, comments)| {
                    Value::Map(vec![
                        (text("id"), Value::from(id)),
                        (text("title"), text(doc.title.as_deref().unwrap_or("Untitled"))),
                        (text("created"), Value::from(doc.created)),
                        (text("edited"), Value::from(doc.edited)),
                        (text("author"), text(&doc.author_hex())),
                        (text("format"), text(&doc.format)),
                        (text("comments"), Value::from(comments)),
                    ])
                })
                .collect();
            lists.push((text(folder), Value::Array(items)));
        }
        packed(&Value::Map(lists))
    }

    /// A new document: made by someone who may manage them (active), or
    /// proposed by someone who may only propose.
    fn work_new(&self, who: &Who, map: &MapRef, scope: &str) -> Result<Response, Refusal> {
        let title = map.str("title").unwrap_or_default().trim().to_string();
        // Stock strips the text before it checks the signature over it.
        let content = map.str("content").unwrap_or_default().trim().to_string();
        let format = format_of(map);
        let signature = who.check(map, content.as_bytes())?;
        if title.chars().count() + content.chars().count() + map.str("format").map_or(8, |f| f.chars().count()) > MAX_DOC_CHARS {
            return Err(refuse(Status::BadRequest, "Content limit exceeded"));
        }
        if title.is_empty() {
            return Err(refuse(Status::BadRequest, "Title is required"));
        }
        if content.is_empty() {
            return Err(refuse(Status::BadRequest, "Content is required"));
        }
        let documents: usize = SCOPES.iter().map(|s| numbers(&who.work.join(s), true).len()).sum();
        if documents >= MAX_DOCUMENTS {
            return Err(refuse(Status::ServerError, "Too many work documents"));
        }
        if dir_size(who.work) + content.len() as u64 > self.quotas.max_repo_bytes {
            return Err(refuse(Status::ServerError, "Work documents have no room left"));
        }
        let id = next_id(who.work);
        let now = now_f64();
        let doc = Doc {
            content,
            title: Some(title),
            format,
            created: now,
            edited: now,
            author: who.me.to_vec(),
            signature: Some(signature.to_vec()),
            identity: who.key.map(|k| k.to_vec()),
        };
        let dir = who.work.join(scope).join(id.to_string());
        std::fs::create_dir_all(&dir).map_err(|_| failed())?;
        if write_file(&dir.join("root"), &doc.encode()).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(refuse(Status::ServerError, "Error saving document"));
        }
        if scope == "proposed" {
            // The author keeps a say over a proposal until it is taken up.
            let me = hex::encode(who.me);
            let owner = format!("i:{me}\nw:{me}\n");
            if write_file(&who.work.join(format!("{id}.allowed")), owner.as_bytes()).is_err() {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(refuse(Status::ServerError, "Error setting document ownership"));
            }
        }
        packed(&Value::Map(vec![(text("id"), Value::from(id)), (text("scope"), text(scope))]))
    }

    fn work_comment(&self, who: &Who, map: &MapRef, doc_id: Option<u64>) -> Result<Response, Refusal> {
        let id = doc_id.ok_or(refuse(Status::BadRequest, "No document ID specified"))?;
        check_scope(map)?;
        let content = map.str("content").unwrap_or_default().trim().to_string();
        if content.chars().count() > MAX_DOC_CHARS {
            return Err(refuse(Status::BadRequest, "Content limit exceeded"));
        }
        if content.is_empty() {
            return Err(refuse(Status::BadRequest, "Content is required"));
        }
        let (_, dir) = find_doc(who.work, id).ok_or(refuse(Status::NotFound, "Document not found"))?;
        let kept = numbers(&dir, false);
        if kept.len() >= MAX_COMMENTS {
            return Err(refuse(Status::ServerError, "Too many comments"));
        }
        if dir_size(who.work) + content.len() as u64 > self.quotas.max_repo_bytes {
            return Err(refuse(Status::ServerError, "Work documents have no room left"));
        }
        let number = kept.into_iter().max().map_or(1, |m| m.saturating_add(1));
        let now = now_f64();
        let comment = Doc {
            content,
            title: None,
            format: format_of(map),
            created: now,
            edited: now,
            author: who.me.to_vec(),
            signature: map.bytes("signature").map(<[u8]>::to_vec),
            identity: None,
        };
        write_file(&dir.join(number.to_string()), &comment.encode()).map_err(|_| refuse(Status::ServerError, "Error saving comment"))?;
        packed(&Value::Map(vec![(text("id"), Value::from(number))]))
    }

    fn work_delete(&self, who: &Who, doc_id: Option<u64>, repo: &access::Grants, group: &access::Grants) -> Result<Response, Refusal> {
        let id = doc_id.ok_or(refuse(Status::BadRequest, "No document ID specified"))?;
        let (_, dir) = find_doc(who.work, id).ok_or(refuse(Status::NotFound, "Document not found"))?;
        let doc = Doc::read(&dir.join("root")).ok_or(refuse(Status::NotFound, "Document not found"))?;
        let admin = access::document(who.me, Perm::Admin, &self.doc_grants(who.work, id), repo, group);
        if doc.author != who.me && !admin {
            return Err(refuse(Status::NotAllowed, "No access, not author"));
        }
        // What decided who may do what with it goes with it.
        let _ = std::fs::remove_file(who.work.join(format!("{id}.allowed")));
        std::fs::remove_dir_all(&dir).map_err(|_| failed())?;
        Ok(ack())
    }

    /// Complete a document, or make one active again (or take up a proposal).
    fn work_move(
        &self,
        who: &Who,
        doc_id: Option<u64>,
        from: &[&str],
        to: &'static str,
        repo: &access::Grants,
        group: &access::Grants,
    ) -> Result<Response, Refusal> {
        let id = doc_id.ok_or(refuse(Status::BadRequest, "No document ID specified"))?;
        let dir = from
            .iter()
            .map(|scope| who.work.join(scope).join(id.to_string()))
            .find(|dir| dir.is_dir())
            .ok_or(refuse(Status::NotFound, "Document not found"))?;
        let doc = Doc::read(&dir.join("root")).ok_or(refuse(Status::ServerError, "Error loading document"))?;
        let admin = access::document(who.me, Perm::Admin, &self.doc_grants(who.work, id), repo, group);
        if doc.author != who.me && !admin {
            return Err(refuse(Status::NotAllowed, "Not allowed"));
        }
        let target = who.work.join(to);
        std::fs::create_dir_all(&target).map_err(|_| failed())?;
        std::fs::rename(&dir, target.join(id.to_string())).map_err(|_| failed())?;
        packed(&Value::Map(vec![(text("id"), Value::from(id)), (text("scope"), text(to))]))
    }

    /// Read or replace the rules of one document. Its author may, if they may
    /// manage documents, and so may its administrators.
    fn work_perms(
        &self,
        who: &Who,
        map: &MapRef,
        doc_id: Option<u64>,
        repo: &access::Grants,
        group: &access::Grants,
    ) -> Result<Response, Refusal> {
        let has = |perm| access::repository(who.me, perm, repo, group);
        if !(has(Perm::Issues) && has(Perm::Write)) {
            return Err(refuse(Status::NotAllowed, "Not allowed"));
        }
        let step = map.str("step").ok_or(refuse(Status::BadRequest, "Invalid request"))?;
        if step != "get" && step != "set" {
            return Err(refuse(Status::BadRequest, "Invalid step"));
        }
        let id = doc_id.ok_or(refuse(Status::BadRequest, "No document ID specified"))?;
        let (_, dir) = find_doc(who.work, id).ok_or(refuse(Status::NotFound, "Document not found"))?;
        let doc = Doc::read(&dir.join("root")).ok_or(refuse(Status::ServerError, "Error loading document"))?;
        let admin = access::document(who.me, Perm::Admin, &self.doc_grants(who.work, id), repo, group);
        // Having got this far, the author may manage documents.
        if doc.author != who.me && !admin {
            return Err(refuse(Status::NotAllowed, "Not allowed"));
        }
        self.perms_step(Some(&step), &who.work.join(format!("{id}.allowed")), map)
    }
}

/// Who is asking, and what of.
struct Who<'a> {
    me: &'a [u8; 16],
    key: Option<&'a [u8; 64]>,
    work: &'a Path,
}

impl Who<'_> {
    /// The signature a request carries over `signed`, if the asker's key
    /// makes it good.
    fn check(&self, map: &MapRef, signed: &[u8]) -> Result<[u8; 64], Refusal> {
        let signature = map.bytes("signature").ok_or(refuse(Status::BadRequest, "No signature provided"))?;
        let signature: [u8; 64] = signature.try_into().map_err(|_| refuse(Status::BadRequest, "Invalid signature length"))?;
        let good =
            self.key.is_some_and(|key| sign::identity_hash(key) == *self.me && sign::verify_signature(key, signed, &signature).is_ok());
        if good { Ok(signature) } else { Err(refuse(Status::BadRequest, "Invalid signature")) }
    }
}

fn check_scope(map: &MapRef) -> Result<(), Refusal> {
    match map.str("scope").as_deref() {
        None | Some("active" | "completed" | "proposed" | "all") => Ok(()),
        Some(_) => Err(refuse(Status::BadRequest, "Invalid request")),
    }
}

fn work_view(work: &Path, map: &MapRef, doc_id: Option<u64>) -> Result<Response, Refusal> {
    check_scope(map)?;
    let id = doc_id.ok_or(refuse(Status::BadRequest, "No document ID specified"))?;
    let (scope, dir) = find_doc(work, id).ok_or(refuse(Status::NotFound, "Not found"))?;
    let doc = Doc::read(&dir.join("root")).ok_or(refuse(Status::NotFound, "Document not found"))?;
    let mut comments: Vec<(u64, Doc)> =
        numbers(&dir, false).into_iter().take(MAX_COMMENTS).filter_map(|n| Some((n, Doc::read(&dir.join(n.to_string()))?))).collect();
    comments.sort_by_key(|(n, _)| *n);
    let comments: Vec<Value> = comments
        .into_iter()
        .map(|(n, c)| {
            Value::Map(vec![
                (text("id"), Value::from(n)),
                (text("content"), text(&c.content)),
                (text("created"), Value::from(c.created)),
                (text("edited"), Value::from(c.edited)),
                (text("author"), text(&c.author_hex())),
                (text("format"), text(&c.format)),
            ])
        })
        .collect();
    let binary = |bytes: &Option<Vec<u8>>| bytes.clone().map_or(Value::Nil, Value::Binary);
    packed(&Value::Map(vec![
        (text("id"), Value::from(id)),
        (text("scope"), text(scope)),
        (text("content"), text(&doc.content)),
        (text("comments"), Value::Array(comments)),
        (
            text("meta"),
            Value::Map(vec![
                (text("title"), text(doc.title.as_deref().unwrap_or("Untitled"))),
                (text("created"), Value::from(doc.created)),
                (text("edited"), Value::from(doc.edited)),
                (text("author"), text(&doc.author_hex())),
                (text("identity"), binary(&doc.identity)),
                (text("signature"), binary(&doc.signature)),
                (text("format"), text(&doc.format)),
            ]),
        ),
    ]))
}

/// Change a document's title and text. Only its author may, and what they
/// send is signed again.
fn work_edit(who: &Who, map: &MapRef, doc_id: Option<u64>) -> Result<Response, Refusal> {
    check_scope(map)?;
    // Unlike a new document's, the text is signed as it comes.
    let content = map.str("content").unwrap_or_default();
    let title = map.str("title").unwrap_or_default();
    let signature = who.check(map, content.as_bytes())?;
    if title.chars().count() + content.chars().count() > MAX_DOC_CHARS {
        return Err(refuse(Status::BadRequest, "Content limit exceeded"));
    }
    if content.is_empty() && title.is_empty() {
        return Err(refuse(Status::BadRequest, "No changes specified"));
    }
    let id = doc_id.filter(|id| *id != 0).ok_or(refuse(Status::BadRequest, "No document ID specified"))?;
    let (_, dir) = find_doc(who.work, id).ok_or(refuse(Status::NotFound, "Not found"))?;
    let root = dir.join("root");
    let mut doc = Doc::read(&root).ok_or(refuse(Status::NotFound, "Document not found"))?;
    if doc.author != who.me {
        return Err(refuse(Status::NotAllowed, "No access, not author"));
    }
    if !title.trim().is_empty() {
        doc.title = Some(title.trim().to_string());
    }
    if !content.trim().is_empty() {
        doc.content = content.trim().to_string();
    }
    doc.edited = now_f64();
    doc.signature = Some(signature.to_vec());
    doc.identity = who.key.map(|k| k.to_vec());
    write_file(&root, &doc.encode()).map_err(|_| refuse(Status::ServerError, "Error saving document"))?;
    Ok(ack())
}
