mod common;

use common::*;
use rmpv::Value;
use rngit::sign::{self, Signer};
use rngit::{Client, Error};

const URL: &str = "rns://727ca184e3d7e83e0e35146940920442/public/demo";

fn s(v: &str) -> Value {
    Value::from(v)
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
}

struct Key(ed25519_dalek::SigningKey, [u8; 64]);

impl Key {
    fn new() -> Self {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let mut public = [7; 64];
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

/// A node that answers the management calls with fixed data.
fn canned(server: std::path::PathBuf) -> MockNode {
    let mut node = MockNode::new(server);
    node.extra = Box::new(|path, req| {
        let op = field(req, "operation").and_then(|v| v.as_str()).unwrap_or("");
        Some(match (path, op) {
            ("/mgmt/release", "list") => ok(&pack(&map(vec![
                (
                    "releases",
                    Value::Array(vec![map(vec![
                        ("tag", s("v1")),
                        ("status", s("published")),
                        ("preview", s("First")),
                        ("artifacts", Value::from(1)),
                    ])]),
                ),
                ("latest", s("v1")),
            ]))),
            ("/mgmt/release", "view") => ok(&pack(&map(vec![
                ("tag", s("v1")),
                ("created", Value::from(1_793_033_000u64)),
                ("notes", s("the notes")),
                ("artifacts", Value::Array(vec![map(vec![("name", s("a.bin")), ("size", Value::from(5))])])),
            ]))),
            ("/mgmt/release", _) => ok(b""),
            ("/mgmt/work", "list") => ok(&pack(&map(vec![(
                "active",
                Value::Array(vec![map(vec![("id", Value::from(1)), ("title", s("A task")), ("comments", Value::from(2))])]),
            )]))),
            ("/mgmt/work", "view") => ok(&pack(&map(vec![
                ("id", Value::from(1)),
                ("scope", s("active")),
                ("content", s("body")),
                ("comments", Value::Array(vec![])),
                ("meta", map(vec![("title", s("A task")), ("author", s("ab"))])),
            ]))),
            ("/mgmt/work", "create" | "propose") => ok(&pack(&map(vec![("id", Value::from(3)), ("scope", s("active"))]))),
            ("/mgmt/work", "comment") => ok(&pack(&map(vec![("id", Value::from(9))]))),
            ("/mgmt/work", "complete" | "activate") => ok(&pack(&map(vec![("id", Value::from(1)), ("scope", s("completed"))]))),
            ("/mgmt/work", _) => ok(b""),
            ("/mgmt/perms", "rperms" | "gperms") if field(req, "step").and_then(|v| v.as_str()) == Some("get") => {
                ok(&pack(&map(vec![("content", s("adm:ff"))])))
            }
            ("/mgmt/perms", _) => ok(b""),
            ("/git/create" | "/git/fork" | "/git/mirror" | "/git/sync" | "/git/delete", _) => ok(b""),
            _ => return None,
        })
    });
    node
}

async fn cli(client: &Client<MockNode>, args: &[&str], stdin: &str) -> (Result<(), Error>, String) {
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let mut out = Vec::new();
    let result = rngit::cli::run(client, &Key::new(), 1_793_033_000, &args, &mut stdin.as_bytes(), &mut out).await;
    (result, String::from_utf8(out).unwrap())
}

fn last(client: &Client<MockNode>) -> (String, Value) {
    client.transport().requests().pop().unwrap()
}

#[tokio::test]
async fn refs_and_clone_work_on_a_real_repository() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server, dest) = (tmp.path().join("work"), tmp.path().join("server.git"), tmp.path().join("dest"));
    work_repo(&work, 2);
    bare_repo(&server);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main"]);
    let client = Client::new(canned(server));

    let (result, text) = cli(&client, &["refs", URL], "").await;
    result.unwrap();
    assert!(text.contains(" refs/heads/main\n") && text.contains("HEAD -> refs/heads/main"), "{text}");

    let (result, text) = cli(&client, &["clone", URL, dest.to_str().unwrap()], "").await;
    result.unwrap();
    assert!(text.starts_with("cloned 1 refs"), "{text}");
    git(&dest, &["fsck", "--strict"]);
    // A directory with something in it is not touched.
    let (result, _) = cli(&client, &["clone", URL, dest.to_str().unwrap()], "").await;
    assert!(matches!(result, Err(Error::InvalidInput(_))));
}

#[tokio::test]
async fn repository_commands_send_the_right_requests() {
    let client = Client::new(canned("/nonexistent".into()));
    for (args, path) in [
        (vec!["create", URL], "/git/create"),
        (vec!["fork", URL, "https://example.com/x.git"], "/git/fork"),
        (vec!["mirror", URL, "https://example.com/x.git"], "/git/mirror"),
        (vec!["sync", URL], "/git/sync"),
        (vec!["delete-ref", URL, "refs/heads/old"], "/git/delete"),
    ] {
        cli(&client, &args, "").await.0.unwrap();
        assert_eq!(last(&client).0, path);
    }
}

#[tokio::test]
async fn permissions_read_write_and_stdin() {
    let client = Client::new(canned("/nonexistent".into()));
    let (result, text) = cli(&client, &["perms", URL], "").await;
    result.unwrap();
    assert_eq!(text, "adm:ff\n");
    cli(&client, &["perms", URL, "r:all"], "").await.0.unwrap();
    assert_eq!(field(&last(&client).1, "content").unwrap().as_str(), Some("r:all"));
    cli(&client, &["gperms", URL, "-"], "w:all\n").await.0.unwrap();
    let (_, request) = last(&client);
    assert_eq!(field(&request, "content").unwrap().as_str(), Some("w:all\n"));
    assert_eq!(field(&request, "operation").unwrap().as_str(), Some("gperms"));
    let (result, text) = cli(&client, &["gperms", URL], "").await;
    result.unwrap();
    assert_eq!(text, "adm:ff\n");
}

#[tokio::test]
async fn release_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let client = Client::new(canned("/nonexistent".into()));
    let (result, text) = cli(&client, &["releases", URL], "").await;
    result.unwrap();
    assert!(text.starts_with("v1 (latest)  published  1 artifacts  First"), "{text}");
    let (result, text) = cli(&client, &["release", URL, "v1"], "").await;
    result.unwrap();
    assert!(text.contains("2026-10-26T16:43:20Z") && text.contains("the notes") && text.contains("a.bin  5 bytes"), "{text}");

    let file = tmp.path().join("app.bin");
    std::fs::write(&file, b"data").unwrap();
    cli(&client, &["release-new", URL, "v2", "notes", file.to_str().unwrap()], "").await.0.unwrap();
    let steps: Vec<_> =
        client.transport().requests().iter().filter_map(|(_, r)| field(r, "step").and_then(|v| v.as_str()).map(String::from)).collect();
    assert!(steps.contains(&"init".to_string()), "{steps:?}");
    cli(&client, &["release-delete", URL, "v2"], "").await.0.unwrap();
    assert_eq!(field(&last(&client).1, "operation").unwrap().as_str(), Some("delete"));

    let (result, _) = cli(&client, &["release-new", URL, "v3", "n", "/nonexistent/file"], "").await;
    assert!(result.is_err());
    // Without signed artifacts on the node, a download can't be verified.
    let (result, _) = cli(&client, &["release-get", URL, "v1", "a.bin", tmp.path().to_str().unwrap()], "").await;
    assert!(result.is_err());
    // The manifest is saved only if its signature checks out.
    let (result, _) = cli(&client, &["release-get", URL, "v1", "manifest.rsm", tmp.path().to_str().unwrap()], "").await;
    assert!(result.is_err());
    assert!(!tmp.path().join("manifest.rsm").exists());
}

#[tokio::test]
async fn work_commands() {
    let client = Client::new(canned("/nonexistent".into()));
    let (result, text) = cli(&client, &["work", URL], "").await;
    result.unwrap();
    assert_eq!(text, "active  #1  A task  (2 comments)\n");
    cli(&client, &["work", URL, "all"], "").await.0.unwrap();
    assert_eq!(field(&last(&client).1, "scope").unwrap().as_str(), Some("all"));

    let (result, text) = cli(&client, &["work-view", URL, "1"], "").await;
    result.unwrap();
    assert!(text.contains("SIGNATURE NOT VERIFIED") && text.contains("body") && text.contains("0 comments"), "{text}");

    let (result, text) = cli(&client, &["work-new", URL, "Title", "-"], "from stdin").await;
    result.unwrap();
    assert_eq!(text, "created #3 in active\n");
    assert_eq!(field(&last(&client).1, "operation").unwrap().as_str(), Some("create"));
    cli(&client, &["work-propose", URL, "Title", "text"], "").await.0.unwrap();
    assert_eq!(field(&last(&client).1, "operation").unwrap().as_str(), Some("propose"));
    let (_, text) = cli(&client, &["work-comment", URL, "1", "hi"], "").await;
    assert_eq!(text, "comment 9\n");
    cli(&client, &["work-edit", URL, "1", "New title", "new text"], "").await.0.unwrap();
    assert_eq!(field(&last(&client).1, "title").unwrap().as_str(), Some("New title"));
    let (_, text) = cli(&client, &["work-complete", URL, "1"], "").await;
    assert_eq!(text, "#1 is now completed\n");
    cli(&client, &["work-activate", URL, "1"], "").await.0.unwrap();
    cli(&client, &["work-delete", URL, "1", "completed"], "").await.0.unwrap();
    assert_eq!(field(&last(&client).1, "scope").unwrap().as_str(), Some("completed"));
}

#[tokio::test]
async fn bad_arguments_are_errors() {
    let client = Client::new(canned("/nonexistent".into()));
    for args in [
        vec!["frobnicate", URL],
        vec!["refs"],
        vec!["refs", "not-a-url"],
        vec!["fork", URL],
        vec!["delete-ref", URL],
        vec!["delete-ref", URL, "main"],
        vec!["work", URL, "sideways"],
        vec!["work-view", URL, "one"],
        vec!["work-new", URL, "only a title"],
    ] {
        let (result, _) = cli(&client, &args, "").await;
        assert!(matches!(result, Err(Error::InvalidInput(_))), "{args:?}: {result:?}");
    }
    let before = client.transport().requests().len();
    for args in [vec![], vec!["help"], vec!["--help"]] {
        let (result, text) = cli(&client, &args, "").await;
        result.unwrap();
        assert!(text.starts_with("usage: rngit"));
    }
    assert_eq!(client.transport().requests().len(), before, "help never touches the network");
}

#[tokio::test]
async fn push_all_reports_what_it_did() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 2);
    git(&work, &["tag", "v1"]);
    bare_repo(&server);
    let client = Client::new(canned(server.clone()));
    let dir = work.to_str().unwrap();

    let (result, text) = cli(&client, &["push-all", URL, dir], "").await;
    result.unwrap();
    assert_eq!(text, "pushed 2, up to date 0, rejected 0, deleted 0\n");
    let (result, text) = cli(&client, &["push-all", URL, dir, "force", "prune"], "").await;
    result.unwrap();
    assert_eq!(text, "pushed 0, up to date 2, rejected 0, deleted 0\n");
    let (result, _) = cli(&client, &["push-all", URL, dir, "everything"], "").await;
    assert!(matches!(result, Err(Error::InvalidInput(_))));
    let (result, _) = cli(&client, &["push-all", URL], "").await;
    assert!(matches!(result, Err(Error::InvalidInput(_))));
}

#[tokio::test]
async fn a_copy_is_read_with_log_tree_and_show() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, copy) = (tmp.path().join("work"), tmp.path().join("copy.git"));
    work_repo(&work, 2);
    std::fs::write(work.join("bin"), [0u8, 159, 146]).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "binary"]);
    bare_repo(&copy);
    git(&work, &["push", "-q", copy.to_str().unwrap(), "main"]);
    let client = Client::new(canned(copy.clone()));
    let dir = copy.to_str().unwrap();

    let (result, text) = cli(&client, &["log", dir, "refs/heads/main", "2"], "").await;
    result.unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() == 2 && lines[0].ends_with(" binary") && lines[1].ends_with(" commit 1"), "{text}");
    let (result, text) = cli(&client, &["tree", dir, "refs/heads/main"], "").await;
    result.unwrap();
    assert!(text.contains("file0.txt  ") && text.contains("bin  3"), "{text}");
    let (result, text) = cli(&client, &["show", dir, "refs/heads/main", "file0.txt"], "").await;
    result.unwrap();
    assert!(text.contains("content 0"), "{text}");
    let (result, text) = cli(&client, &["show", dir, "refs/heads/main", "bin"], "").await;
    result.unwrap();
    assert_eq!(text, "binary file, 3 bytes\n");
    for args in [&["log", dir][..], &["log", dir, "refs/heads/main", "many"], &["show", dir, "refs/heads/main"], &["tree", dir, "main"]] {
        let (result, _) = cli(&client, args, "").await;
        assert!(matches!(result, Err(Error::InvalidInput(_))), "{args:?}");
    }
}

#[tokio::test]
async fn release_latest_is_sent_and_a_bad_tag_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let client = Client::new(canned(tmp.path().join("server.git")));
    let (result, _) = cli(&client, &["release-latest", URL, "v1"], "").await;
    result.unwrap();
    let (path, request) = last(&client);
    assert_eq!(path, "/mgmt/release");
    assert_eq!(field(&request, "operation").and_then(|v| v.as_str()), Some("latest"));
    assert_eq!(field(&request, "tag").and_then(|v| v.as_str()), Some("v1"));
    let before = client.transport().requests().len();
    for bad in ["a/b", "..", ""] {
        let (result, _) = cli(&client, &["release-latest", URL, bad], "").await;
        assert!(matches!(result, Err(Error::InvalidInput(_))), "{bad:?}");
    }
    let (result, _) = cli(&client, &["release-latest", URL], "").await;
    assert!(matches!(result, Err(Error::InvalidInput(_))));
    assert_eq!(client.transport().requests().len(), before);
}

#[tokio::test]
async fn a_downloaded_release_is_verified_without_a_node() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("rel");
    std::fs::create_dir_all(&dir).unwrap();
    let key = Key::new();
    let files = [("app.bin", b"the app".to_vec()), ("notes.txt", b"read me".to_vec())];
    let artifacts: Vec<(String, Vec<u8>)> =
        files.iter().map(|(name, data)| (name.to_string(), sign::make_rsg(&key, data, 10).unwrap())).collect();
    let rsm = sign::make_rsm(
        &key,
        &sign::ManifestInput {
            name: "demo.git",
            version: "v1",
            released: "2026-01-01T00:00:00Z",
            origin: [1; 16],
            path: "public/demo.git",
            commit: "",
            message: b"notes",
            artifacts: &artifacts,
        },
        10,
    )
    .unwrap();
    std::fs::write(dir.join("demo.git_v1.rsm"), &rsm).unwrap();
    for (name, data) in &files {
        std::fs::write(dir.join(name), data).unwrap();
    }
    let client = Client::new(canned(tmp.path().join("server.git")));
    let manifest = dir.join("demo.git_v1.rsm");
    let manifest = manifest.to_str().unwrap();

    let (result, text) = cli(&client, &["verify", manifest], "").await;
    result.unwrap();
    assert!(
        text.contains("File app.bin validated") && text.contains("File notes.txt validated") && text.ends_with("All files validated\n"),
        "{text}"
    );
    assert!(client.transport().requests().is_empty(), "no node was asked");

    // A file that changed, one that is missing and a manifest that was changed.
    std::fs::write(dir.join("app.bin"), b"tampered").unwrap();
    std::fs::remove_file(dir.join("notes.txt")).unwrap();
    let (result, text) = cli(&client, &["verify", manifest], "").await;
    assert!(matches!(result, Err(Error::Verify(_))));
    assert!(text.contains("app.bin does not match") && text.contains("notes.txt from the manifest does not exist"), "{text}");
    // The files can be in another folder than the manifest.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("app.bin"), b"the app").unwrap();
    std::fs::write(elsewhere.join("notes.txt"), b"read me").unwrap();
    let (result, _) = cli(&client, &["verify", manifest, elsewhere.to_str().unwrap()], "").await;
    result.unwrap();
    let mut bad = rsm.clone();
    bad[100] ^= 1;
    std::fs::write(dir.join("bad.rsm"), bad).unwrap();
    let (result, _) = cli(&client, &["verify", dir.join("bad.rsm").to_str().unwrap()], "").await;
    assert!(matches!(result, Err(Error::Verify(_))));
    let (result, _) = cli(&client, &["verify", "/no/such/file.rsm"], "").await;
    assert!(result.is_err());
}
