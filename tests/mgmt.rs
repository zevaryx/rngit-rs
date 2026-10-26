mod common;

use common::*;
use rmpv::Value;
use rngit::client::{WorkKind, WorkScope};
use rngit::sign::{self, Signer};
use rngit::{Client, Error, RepoUrl, Response, Status};

fn url() -> RepoUrl {
    "rns://727ca184e3d7e83e0e35146940920442/public/demo".parse().unwrap()
}

fn s(v: &str) -> Value {
    Value::from(v)
}

fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
}

fn node(extra: impl Fn(&str, &Value) -> Option<Response> + Send + Sync + 'static) -> MockNode {
    let mut node = MockNode::new(std::path::PathBuf::from("/nonexistent"));
    node.extra = Box::new(extra);
    node
}

struct Key(ed25519_dalek::SigningKey, [u8; 64]);

impl Key {
    fn new(seed: u8) -> Self {
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let mut public = [seed; 64];
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

#[tokio::test]
async fn reads_release_list_and_detail() {
    let client = Client::new(node(|path, req| {
        assert_eq!(path, "/mgmt/release");
        let op = field(req, "operation")?.as_str()?;
        Some(match op {
            "list" => ok(&pack(&map(vec![
                (
                    "releases",
                    Value::Array(vec![map(vec![
                        ("tag", s("v1.0.0")),
                        ("hash", s("abc")),
                        ("created", Value::from(1_700_000_000u64)),
                        ("status", s("published")),
                        ("created_by", s("ff")),
                        ("preview", s("First")),
                        ("format", s("markdown")),
                        ("artifacts", Value::from(2)),
                    ])]),
                ),
                ("latest", s("v1.0.0")),
            ]))),
            "view" => ok(&pack(&map(vec![
                ("tag", s("v1.0.0")),
                ("notes", s("# Notes")),
                ("notes_format", s("markdown")),
                ("artifacts", Value::Array(vec![map(vec![("name", s("a.bin")), ("size", Value::from(5))])])),
            ]))),
            _ => status(3, "Release not found"),
        })
    }));
    let list = client.releases(&url()).await.unwrap();
    assert_eq!(list.latest.as_deref(), Some("v1.0.0"));
    assert_eq!(list.releases[0].artifacts, 2);
    assert_eq!(list.releases[0].created, 1_700_000_000);
    let detail = client.release(&url(), "v1.0.0").await.unwrap();
    assert_eq!(detail.notes, "# Notes");
    assert_eq!(detail.artifacts[0].size, 5);
}

#[tokio::test]
async fn release_list_ignores_junk_entries() {
    let client = Client::new(node(|_, _| {
        Some(ok(&pack(&map(vec![("releases", Value::Array(vec![Value::from(1), Value::Nil, map(vec![("no_tag", Value::from(1))])]))]))))
    }));
    let list = client.releases(&url()).await.unwrap();
    assert!(list.releases.is_empty());
    assert_eq!(list.latest, None);
}

#[tokio::test]
async fn missing_release_is_not_found() {
    let client = Client::new(node(|_, _| Some(status(3, "Release not found"))));
    match client.release(&url(), "nope").await {
        Err(Error::Remote { status: Status::NotFound, message }) => assert_eq!(message, "Release not found"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn artifact_names_with_separators_are_refused() {
    let client = Client::new(node(|_, _| panic!("nothing should be sent")));
    for bad in ["../x", "a/b", "a\\b", "", "..", "x\0"] {
        assert!(matches!(client.fetch_artifact(&url(), "v1", bad).await, Err(Error::InvalidInput(_))), "{bad:?}");
    }
}

#[tokio::test]
async fn release_create_follows_the_three_phases_and_signs_everything() {
    let client = Client::new(node(|_, _| Some(ok(b""))));
    let key = Key::new(9);
    let files = vec![("a.bin".to_string(), b"AAAA".to_vec()), ("b.bin".to_string(), b"BB".to_vec())];
    client.create_release(&url(), &key, "v1.0.0", Some("deadbeef"), "notes", &files, "2026-10-26T00:00:00Z", 1_700_000_000).await.unwrap();

    let log = client.transport().requests();
    let steps: Vec<String> = log.iter().map(|(_, r)| field(r, "step").unwrap().as_str().unwrap().to_string()).collect();
    assert_eq!(steps, ["init", "artifact", "artifact", "artifact", "artifact", "artifact", "finalize"]);
    assert_eq!(field(&log[0].1, "hash").unwrap().as_str(), Some("deadbeef"));
    assert_eq!(field(&log[0].1, "notes_format").unwrap().as_str(), Some("markdown"));

    // What the node would now hold verifies as a release.
    let mut uploaded = Vec::new();
    for (_, r) in &log[1..6] {
        let Some(Value::Binary(data)) = field(r, "artifact_data") else { panic!() };
        uploaded.push((field(r, "artifact_name").unwrap().as_str().unwrap().to_string(), data.clone()));
    }
    assert_eq!(uploaded[0].0, "manifest.rsm");
    let manifest = sign::verify_release_files(&uploaded[0].1, &uploaded[1..]).unwrap();
    assert_eq!(manifest.version, "v1.0.0");
    assert_eq!(manifest.signer.signer, key.identity_hash());
    assert_eq!(manifest.origin, Some(url().destination));
    assert_eq!(manifest.artifacts.len(), 2);
}

#[tokio::test]
async fn release_create_refuses_reserved_names_before_init() {
    let client = Client::new(node(|_, _| panic!("nothing should be sent")));
    let key = Key::new(1);
    for name in ["manifest.rsm", "x.rsg"] {
        let files = vec![(name.to_string(), vec![1])];
        assert!(client.create_release(&url(), &key, "v1", None, "", &files, "", 0).await.is_err());
    }
}

#[tokio::test]
async fn release_create_stops_when_the_tag_is_missing() {
    let client = Client::new(node(|_, _| Some(status(2, "Tag 'v9' does not exist in repository"))));
    let key = Key::new(1);
    let err = client.create_release(&url(), &key, "v9", None, "", &[], "", 0).await.unwrap_err();
    assert!(matches!(err, Error::Remote { status: Status::BadRequest, .. }));
    assert_eq!(client.transport().requests().len(), 1);
}

#[tokio::test]
async fn fetch_verified_release_checks_signatures() {
    let key = Key::new(5);
    let data = b"hello artifact".to_vec();
    let rsg = sign::make_rsg(&key, &data, 10).unwrap();
    let artifacts = [("hello.txt".to_string(), rsg.clone())];
    let rsm = sign::make_rsm(
        &key,
        &sign::ManifestInput {
            name: "demo.git",
            version: "v1",
            released: "r",
            origin: [0; 16],
            path: "public/demo.git",
            commit: "c",
            message: b"n",
            artifacts: &artifacts,
        },
        10,
    )
    .unwrap();

    let serve = |tamper: bool| {
        let (rsm, data) = (rsm.clone(), data.clone());
        node(move |_, req| {
            let name = field(req, "artifact")?.as_str()?;
            let mut body = if name == "manifest.rsm" { rsm.clone() } else { data.clone() };
            if tamper && name == "hello.txt" {
                body[0] ^= 1;
            }
            Some(Response::File { data: body, metadata: None })
        })
    };
    let (manifest, files) = Client::new(serve(false)).fetch_verified_release(&url(), "latest").await.unwrap();
    assert_eq!(manifest.signer.signer, key.identity_hash());
    assert_eq!(files[0].1, data);
    assert!(matches!(Client::new(serve(true)).fetch_verified_release(&url(), "latest").await, Err(Error::Verify(_))));
}

#[tokio::test]
async fn work_documents_round_trip_with_signature_check() {
    let key = Key::new(6);
    let store = std::sync::Mutex::new(None::<(String, Vec<u8>)>);
    let public = key.public_key();
    let client = Client::new(node(move |_, req| {
        Some(match field(req, "operation")?.as_str()? {
            "create" | "propose" => {
                let Value::Binary(sig) = field(req, "signature")? else { return Some(status(2, "Invalid request")) };
                *store.lock().unwrap() = Some((field(req, "content")?.as_str()?.to_string(), sig.clone()));
                ok(&pack(&map(vec![("id", Value::from(3)), ("scope", s("active"))])))
            }
            "view" => {
                let (content, sig) = store.lock().unwrap().clone().unwrap();
                ok(&pack(&map(vec![
                    ("id", Value::from(3)),
                    ("scope", s("active")),
                    ("content", s(&content)),
                    ("comments", Value::Array(vec![])),
                    (
                        "meta",
                        map(vec![
                            ("title", s("Doc")),
                            ("created", Value::F64(1.5)),
                            ("edited", Value::F64(2.5)),
                            ("author", Value::Binary(vec![0xab; 16])),
                            ("identity", Value::Binary(public.to_vec())),
                            ("signature", Value::Binary(sig)),
                            ("format", s("markdown")),
                        ]),
                    ),
                ])))
            }
            "list" => ok(&pack(&map(vec![
                (
                    "active",
                    Value::Array(vec![map(vec![
                        ("id", Value::from(3)),
                        ("title", s("Doc")),
                        ("created", Value::F64(1.5)),
                        ("comments", Value::from(2)),
                    ])]),
                ),
                ("completed", Value::Array(vec![])),
                ("proposed", Value::Array(vec![])),
            ]))),
            "comment" => ok(&pack(&map(vec![("id", Value::from(1))]))),
            "complete" => ok(&pack(&map(vec![("id", Value::from(3)), ("scope", s("completed"))]))),
            "activate" => ok(&pack(&map(vec![("id", Value::from(3)), ("scope", s("active"))]))),
            "edit" | "delete" => ok(b""),
            _ => status(2, "Invalid request"),
        })
    }));

    let (id, scope) = client.work_create(&url(), &key, WorkKind::Create, "Doc", "Body text", "markdown").await.unwrap();
    assert_eq!((id, scope.as_str()), (3, "active"));
    let doc = client.work_view(&url(), id, WorkScope::Active).await.unwrap();
    assert!(doc.verified);
    assert_eq!(doc.content, "Body text");
    assert_eq!(doc.author, "ab".repeat(16));
    assert_eq!(doc.created, 1.5);

    let list = client.work_list(&url(), WorkScope::All).await.unwrap();
    assert_eq!(list.active.len(), 1);
    assert_eq!(list.active[0].comments, 2);
    assert_eq!(client.work_comment(&url(), id, WorkScope::Active, "hi", "markdown").await.unwrap(), 1);
    assert_eq!(client.work_complete(&url(), id).await.unwrap().1, "completed");
    assert_eq!(client.work_activate(&url(), id).await.unwrap().1, "active");
    client.work_edit(&url(), &key, id, WorkScope::Active, "Doc", "New body").await.unwrap();
    client.work_delete(&url(), id, WorkScope::Active).await.unwrap();

    // The signature covers the content only; the edit signs the new content.
    let log = client.transport().requests();
    let edit = log.iter().find(|(_, r)| field(r, "operation").and_then(|v| v.as_str()) == Some("edit")).unwrap();
    let Some(Value::Binary(sig)) = field(&edit.1, "signature") else { panic!() };
    sign::verify_signature(&key.public_key(), b"New body", sig).unwrap();
}

#[tokio::test]
async fn work_view_flags_a_bad_signature() {
    let key = Key::new(6);
    let client = Client::new(node(move |_, _| {
        Some(ok(&pack(&map(vec![
            ("content", s("Tampered")),
            ("meta", map(vec![("identity", Value::Binary(key.public_key().to_vec())), ("signature", Value::Binary(vec![0; 64]))])),
        ]))))
    }));
    assert!(!client.work_view(&url(), 1, WorkScope::Active).await.unwrap().verified);
}

#[tokio::test]
async fn work_propose_uses_the_propose_operation() {
    let client = Client::new(node(|_, _| Some(ok(&pack(&map(vec![("id", Value::from(8)), ("scope", s("proposed"))]))))));
    let key = Key::new(2);
    let (id, scope) = client.work_create(&url(), &key, WorkKind::Propose, "T", "C", "markdown").await.unwrap();
    assert_eq!((id, scope.as_str()), (8, "proposed"));
    assert_eq!(field(&client.transport().requests()[0].1, "operation").unwrap().as_str(), Some("propose"));
}

#[tokio::test]
async fn permissions_get_and_set_for_repo_group_and_doc() {
    let content = std::sync::Mutex::new("r:all\n".to_string());
    let client = Client::new(node(move |_, req| {
        let step = field(req, "step")?.as_str()?;
        Some(if step == "get" {
            ok(&pack(&map(vec![("content", s(&content.lock().unwrap()))])))
        } else {
            *content.lock().unwrap() = field(req, "content")?.as_str()?.to_string();
            ok(b"")
        })
    }));
    assert_eq!(client.repo_perms(&url()).await.unwrap(), "r:all\n");
    client.set_repo_perms(&url(), "adm:all\n").await.unwrap();
    assert_eq!(client.group_perms("public").await.unwrap(), "adm:all\n");
    client.set_group_perms("public", "r:all\n").await.unwrap();
    assert_eq!(client.work_perms(&url(), 3).await.unwrap(), "r:all\n");
    client.set_work_perms(&url(), 3, "w:none\n").await.unwrap();

    let log = client.transport().requests();
    // The group goes in integer key 2 and no repository key is sent.
    let group_get = &log[2].1;
    let Value::Map(entries) = group_get else { panic!() };
    assert!(entries.iter().any(|(k, v)| k.as_u64() == Some(2) && v.as_str() == Some("public")));
    assert!(!entries.iter().any(|(k, _)| k.as_u64() == Some(0)));
    assert_eq!(field(group_get, "operation").unwrap().as_str(), Some("gperms"));
    assert_eq!(field(&log[0].1, "operation").unwrap().as_str(), Some("rperms"));
}

#[tokio::test]
async fn invalid_permissions_syntax_surfaces_the_nodes_text() {
    let client = Client::new(node(|_, _| Some(status(2, "Invalid permission syntax on line 1"))));
    match client.set_repo_perms(&url(), "garbage").await {
        Err(Error::Remote { status: Status::BadRequest, message }) => assert!(message.contains("line 1")),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_file_reply_where_a_packet_was_expected_is_a_protocol_error() {
    let client = Client::new(node(|_, _| Some(Response::File { data: vec![1, 2, 3], metadata: None })));
    assert!(matches!(client.releases(&url()).await, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn an_oversize_reply_is_refused() {
    let client =
        Client::new(node(|_, _| Some(ok(&vec![0u8; 5000])))).with_limits(rngit::Limits { max_reply_bytes: 1000, ..Default::default() });
    assert!(matches!(client.releases(&url()).await, Err(Error::TooLarge { .. })));
}

#[tokio::test]
async fn garbage_messagepack_is_a_protocol_error_not_a_panic() {
    let client = Client::new(node(|_, _| Some(ok(&[0x85, 0xa1]))));
    assert!(matches!(client.releases(&url()).await, Err(Error::Protocol(_))));
    assert!(matches!(client.work_list(&url(), WorkScope::All).await, Err(Error::Protocol(_))));
}

#[tokio::test]
async fn ref_list_parsing_rejects_bad_lines_and_caps_size() {
    use rngit::client::parse_ref_list;
    let sha = "a".repeat(40);
    assert!(parse_ref_list(format!("{sha} refs/heads/main\n@refs/heads/main HEAD\n").as_bytes(), 10).is_ok());
    assert!(parse_ref_list(b"nonsense\n", 10).is_err());
    assert!(parse_ref_list(b"zz refs/heads/main\n", 10).is_err());
    assert!(parse_ref_list(b"\xff\xfe", 10).is_err());
    let two = format!("{sha} refs/heads/a\n{sha} refs/heads/b\n");
    assert!(matches!(parse_ref_list(two.as_bytes(), 1), Err(Error::TooLarge { .. })));
    // An empty repository: just the HEAD line.
    let empty = parse_ref_list(b"@refs/heads/master HEAD\n", 10).unwrap();
    assert!(empty.refs.is_empty());
    assert_eq!(empty.head.as_deref(), Some("refs/heads/master"));
}

#[tokio::test]
async fn a_repository_without_releases_answers_with_a_bare_empty_array() {
    let client = Client::new(node(|_, _| Some(ok(&pack(&Value::Array(vec![]))))));
    let list = client.releases(&url()).await.unwrap();
    assert!(list.releases.is_empty() && list.latest.is_none());
    // Any other non-map reply is still an error.
    let client = Client::new(node(|_, _| Some(ok(&pack(&Value::Array(vec![Value::from(1)]))))));
    assert!(client.releases(&url()).await.is_err());
}
