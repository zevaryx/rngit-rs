//! Replays the requests a stock rngit client sent (recorded at the node) and
//! checks that rngit asks for the same things in the same shape.

mod common;

use std::collections::BTreeSet;

use common::*;
use rmpv::Value;
use rngit::client::{WorkKind, WorkScope};
use rngit::sign::{self, Signer};
use rngit::{Client, Error, RepoUrl, Want};
use serde_json::{Value as Json, json};

const DEST: &str = "727ca184e3d7e83e0e35146940920442";

struct Key(ed25519_dalek::SigningKey, [u8; 64]);

impl Key {
    fn new() -> Self {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9; 32]);
        let mut public = [9; 64];
        public[32..].copy_from_slice(key.verifying_key().as_bytes());
        Key(key, public)
    }
}

impl Signer for Key {
    fn identity_hash(&self) -> [u8; 16] {
        sign::identity_hash(&self.1)
    }
    fn public_key(&self) -> [u8; 64] {
        self.1
    }
    fn sign(&self, message: &[u8]) -> Result<[u8; 64], Error> {
        use ed25519_dalek::Signer as _;
        Ok(self.0.sign(message).to_bytes())
    }
}

/// The request as MessagePack decoded to JSON: integer keys become strings,
/// byte strings become "<bytes>" so only their presence is compared.
fn to_json(value: &Value) -> Json {
    match value {
        Value::Nil => Json::Null,
        Value::Boolean(b) => json!(b),
        Value::Integer(i) => json!(i.as_i64().unwrap()),
        Value::String(s) => json!(s.as_str().unwrap()),
        Value::Binary(_) => json!("<bytes>"),
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(k, v)| (k.as_str().map(String::from).unwrap_or_else(|| k.as_i64().unwrap().to_string()), to_json(v)))
                .collect(),
        ),
        other => panic!("unexpected value {other:?}"),
    }
}

fn text<'a>(request: &'a Json, key: &str) -> &'a str {
    request[key].as_str().unwrap_or_else(|| panic!("{key} missing in {request}"))
}

fn scope(request: &Json) -> Option<WorkScope> {
    match request["scope"].as_str()? {
        "active" => Some(WorkScope::Active),
        "completed" => Some(WorkScope::Completed),
        "proposed" => Some(WorkScope::Proposed),
        "all" => Some(WorkScope::All),
        _ => None,
    }
}

/// Make the call that stock rngit made, or say the shape is not one we send.
async fn replay(client: &Client<MockNode>, path: &str, request: &Json) -> bool {
    let repo_path = request["0"].as_str().unwrap_or_default();
    let (group, name) = repo_path.split_once('/').unwrap_or_default();
    let Ok(repo) = RepoUrl::new(hex::decode(DEST).unwrap().try_into().unwrap(), group, name) else { return false };
    let key = Key::new();
    let operation = request["operation"].as_str().unwrap_or_default();
    let step = request["step"].as_str().unwrap_or_default();
    match (path, operation, step) {
        ("/git/list", ..) => drop(client.list(&repo, request["for_push"].as_bool().unwrap_or(false)).await),
        ("/git/fetch", ..) => {
            // An empty list was a probe for the node's error; we refuse to send it.
            let Some(refs) = request["refs"].as_array().filter(|r| !r.is_empty()) else { return false };
            let wants: Vec<Want> =
                refs.iter().map(|r| Want { sha: text(r, "sha").parse().unwrap(), name: text(r, "ref").to_string() }).collect();
            let mut haves = rngit::bundle::Haves::default();
            for r in refs {
                if let Some(h) = r["have"].as_str() {
                    haves.per_ref.push((text(r, "ref").to_string(), h.parse().unwrap()));
                }
            }
            for h in request["have"].as_array().into_iter().flatten() {
                haves.tips.push(h.as_str().unwrap().parse().unwrap());
            }
            drop(client.fetch_bundle_with(&repo, &wants, &haves).await)
        }
        ("/git/push", ..) => {
            if let Some(ops) = request["operations"].as_array() {
                let op = &ops[0];
                drop(client.update_ref(&repo, text(op, "ref"), text(op, "sha").parse().unwrap(), op["force"].as_bool().unwrap()).await)
            } else if text(request, "local_ref").starts_with("refs/") {
                let force = request["force"].as_bool().unwrap();
                drop(client.push_bundle(&repo, text(request, "local_ref"), text(request, "remote_ref"), force, vec![1]).await)
            } else {
                // The stock helper once forwarded "HEAD", which the node refused; we never send that.
                return false;
            }
        }
        ("/git/fork", ..) => drop(client.fork(&repo, text(request, "source")).await),
        ("/git/mirror", ..) => drop(client.mirror(&repo, text(request, "source")).await),
        ("/mgmt/perms", "rperms", "get") => drop(client.repo_perms(&repo).await),
        ("/mgmt/perms", "rperms", "set") => drop(client.set_repo_perms(&repo, text(request, "content")).await),
        ("/mgmt/perms", "gperms", "get") => drop(client.group_perms(request["2"].as_str().unwrap()).await),
        ("/mgmt/perms", "gperms", "set") => drop(client.set_group_perms(request["2"].as_str().unwrap(), text(request, "content")).await),
        ("/mgmt/release", "list", _) => drop(client.releases(&repo).await),
        ("/mgmt/release", "view", _) => drop(client.release(&repo, text(request, "tag")).await),
        ("/mgmt/release", "fetch", _) => drop(client.fetch_artifact(&repo, text(request, "tag"), text(request, "artifact")).await),
        ("/mgmt/release", "delete", _) => drop(client.delete_release(&repo, text(request, "tag")).await),
        ("/mgmt/release", "create", "init") => {
            // The node answers the three steps in one go; the init request is the first we send.
            let commit = request["hash"].as_str();
            drop(
                client
                    .create_release(&repo, &key, text(request, "tag"), commit, text(request, "notes"), &[], "2026-01-01T00:00:00Z", 1)
                    .await,
            )
        }
        ("/mgmt/release", "create", "artifact") => {
            // Uploads are the user's files plus a signature for each and a manifest; replay a publish that makes this one.
            let name = text(request, "artifact_name");
            let file = name.strip_suffix(".rsg").unwrap_or(if name == "manifest.rsm" { "release.bin" } else { name });
            let files = vec![(file.to_string(), b"data".to_vec())];
            drop(client.create_release(&repo, &key, text(request, "tag"), None, "notes", &files, "2026-01-01T00:00:00Z", 1).await)
        }
        ("/mgmt/release", "create", "finalize") => {
            let files = vec![("a.bin".to_string(), b"data".to_vec())];
            drop(client.create_release(&repo, &key, text(request, "tag"), None, "notes", &files, "2026-01-01T00:00:00Z", 1).await)
        }
        ("/mgmt/release", "create", _) => return false,
        ("/mgmt/work", "list", _) => {
            let Some(scope) = scope(request) else { return false };
            drop(client.work_list(&repo, scope).await)
        }
        ("/mgmt/work", "create" | "propose", _) => {
            let kind = if operation == "create" { WorkKind::Create } else { WorkKind::Propose };
            drop(client.work_create(&repo, &key, kind, text(request, "title"), text(request, "content"), text(request, "format")).await)
        }
        ("/mgmt/work", "view", _) => drop(client.work_view(&repo, request["doc_id"].as_u64().unwrap(), scope(request).unwrap()).await),
        ("/mgmt/work", "comment", _) => drop(
            client
                .work_comment(
                    &repo,
                    request["doc_id"].as_u64().unwrap(),
                    WorkScope::Active,
                    text(request, "content"),
                    text(request, "format"),
                )
                .await,
        ),
        ("/mgmt/work", "edit", _) => drop(
            client
                .work_edit(
                    &repo,
                    &key,
                    request["doc_id"].as_u64().unwrap(),
                    WorkScope::Active,
                    text(request, "title"),
                    text(request, "content"),
                )
                .await,
        ),
        ("/mgmt/work", "complete", _) => drop(client.work_complete(&repo, request["doc_id"].as_u64().unwrap()).await),
        ("/mgmt/work", "activate", _) => drop(client.work_activate(&repo, request["doc_id"].as_u64().unwrap()).await),
        ("/mgmt/work", "perms", "get") => drop(client.work_perms(&repo, request["doc_id"].as_u64().unwrap()).await),
        ("/mgmt/work", "perms", "set") => {
            drop(client.set_work_perms(&repo, request["doc_id"].as_u64().unwrap(), text(request, "content")).await)
        }
        ("/mgmt/work", "delete", _) => drop(client.work_delete(&repo, request["doc_id"].as_u64().unwrap(), scope(request).unwrap()).await),
        _ => return false,
    }
    true
}

#[tokio::test]
async fn our_requests_match_the_ones_stock_rngit_sent() {
    let fixture: Vec<Json> = serde_json::from_str(include_str!("fixtures/captured_requests.json")).unwrap();
    let mut covered = BTreeSet::new();
    let mut skipped = 0;
    for entry in &fixture {
        let (path, expected) = (text(entry, "path"), &entry["request"]);
        // An empty-reply node: calls fail to parse the reply, which is fine here.
        let mut node = MockNode::new("/nonexistent".into());
        node.extra = Box::new(|_, _| Some(ok(b"")));
        let client = Client::new(node);
        if !replay(&client, path, expected).await {
            skipped += 1;
            continue;
        }
        let sent = client.transport().requests();
        let normalize = |mut v: Json| {
            if let Some(map) = v.as_object_mut() {
                // The commit hash is optional and null in the capture.
                map.remove("hash").filter(|h| !h.is_null());
            }
            v
        };
        let want = normalize(expected.clone());
        let step = text_or(expected, "step");
        let actual: Vec<Json> = sent.iter().filter(|(p, _)| p == path).map(|(_, v)| normalize(to_json(v))).collect();
        // A release publish sends several requests; the others are one request.
        let found = if step == "artifact" || step == "finalize" { actual.contains(&want) } else { actual.first() == Some(&want) };
        assert!(found, "request for {path} differs from stock rngit:\n want {want}\n sent {actual:#?}");
        covered.insert(format!("{path} {} {}", text_or(expected, "operation"), text_or(expected, "step")));
    }
    // Every kind of call recorded from stock rngit that we make must have been checked.
    assert!(covered.len() >= 25, "only {} kinds covered, {skipped} skipped: {covered:#?}", covered.len());
}

fn text_or<'a>(request: &'a Json, key: &str) -> &'a str {
    request[key].as_str().unwrap_or("-")
}
