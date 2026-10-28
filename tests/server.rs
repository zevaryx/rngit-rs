mod common;

use common::*;
use rngit::error::{Error, Status};
use rngit::server::{Follows, NULL_IDENT, Node, Rule, Sender, Who};
use rngit::{Client, RepoUrl, Response, Transport};

const ME: [u8; 16] = [7; 16];

/// A client's view of a node in this process: requests go straight to it.
struct Direct {
    node: Node,
    peer: Option<[u8; 16]>,
    /// The public key of the identity that sends, if it is known.
    key: Option<[u8; 64]>,
}

impl Transport for Direct {
    async fn request(&self, path: &str, data: Vec<u8>) -> Result<Response, Error> {
        self.node
            .handle_from(path, &data, self.peer.map(|hash| Sender { hash, key: self.key }))
            .ok_or_else(|| Error::Transport("unknown path".into()))
    }
}

/// A node with `public/demo.git` holding two branches and two tags.
fn node(rules: &str) -> (tempfile::TempDir, Node, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    work_repo(&work, 2);
    git(&work, &["tag", "v1"]);
    git(&work, &["tag", "-a", "-m", "annotated", "v2"]);
    git(&work, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(work.join("t.txt"), "t").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "on topic"]);
    let repo = tmp.path().join("repos/public/demo.git");
    bare_repo(&repo);
    git(&work, &["push", "-q", repo.to_str().unwrap(), "main", "topic", "--tags"]);
    std::fs::write(tmp.path().join("repos/public/demo.git.allowed"), rules).unwrap();
    let node = Node::new(tmp.path().join("repos"));
    (tmp, node, work)
}

fn url() -> RepoUrl {
    "rns://00112233445566778899aabbccddeeff/public/demo".parse().unwrap()
}

/// rsReticulum sends a reply that is one whole MessagePack value as that value, not as bytes.
fn whole_msgpack(bytes: &[u8]) -> bool {
    let mut cursor = std::io::Cursor::new(bytes);
    rmpv::decode::read_value(&mut cursor).is_ok() && cursor.position() as usize == bytes.len()
}

fn text(response: &Response) -> (u8, String) {
    match response {
        Response::Packet(p) => {
            assert!(!whole_msgpack(p), "{p:?} would be sent as a number, not as bytes");
            (p[0], String::from_utf8_lossy(&p[1..]).into_owned())
        }
        Response::File { .. } => (0, "file".into()),
    }
}

#[test]
fn rules_read_both_separators_and_skip_what_they_cannot() {
    let rules = Rule::parse(&format!("rw:all, c:all\nadm:{}\nx:all\nr:nothex\n\nr:none", "ab".repeat(16)));
    assert_eq!(rules.len(), 4);
    assert_eq!(rules[2].who, Who::Identity([0xab; 16]));
    assert!(rules[3].who == Who::Nobody);
}

#[tokio::test]
async fn a_client_clones_and_then_fetches_only_what_is_new() {
    let (tmp, node, work) = node("r:all");
    let client = Client::new(Direct { node, peer: Some(ME), key: None });
    let dest = tmp.path().join("clone.git");
    let refs = client.clone_into(&url(), &dest).await.unwrap();
    assert_eq!(refs.head.as_deref(), Some("refs/heads/main"));
    let names: Vec<&str> = refs.refs.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(names, ["refs/heads/main", "refs/heads/topic", "refs/tags/v1", "refs/tags/v2"]);
    assert_eq!(git(&dest, &["rev-parse", "topic"]), git(&work, &["rev-parse", "topic"]));
    git(&dest, &["fsck", "--strict"]);

    // A new commit on the node comes as a small bundle that builds on the clone.
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("more.txt"), "more").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "more"]);
    git(&work, &["push", "-q", tmp.path().join("repos/public/demo.git").to_str().unwrap(), "main"]);
    let refs = client.list(&url(), false).await.unwrap();
    let wants: Vec<_> = refs.refs.iter().map(|(sha, name)| rngit::Want { sha: *sha, name: name.clone() }).collect();
    client.fetch_into(&url(), &wants, &dest).await.unwrap();
    assert_eq!(git(&dest, &["rev-parse", "main"]), git(&work, &["rev-parse", "main"]));
    git(&dest, &["fsck", "--strict"]);
}

#[test]
fn who_may_read_is_decided_by_the_rules() {
    let (tmp, mut node, _) = node(&format!("r:{}", hex::encode(ME)));
    let list = rmpv_request("public/demo.git", &[]);
    // The named identity reads, another one and an unidentified peer see nothing.
    assert_eq!(text(&node.handle("/git/list", &list, Some(ME)).unwrap()).0, 0);
    assert_eq!(text(&node.handle("/git/list", &list, Some([9; 16])).unwrap()), (3, "Not found".into()));
    assert_eq!(text(&node.handle("/git/list", &list, None).unwrap()).0, 3);
    // A group rule from the node's settings doesn't reach a repository that names who reads it, as in stock rngit...
    node.group_rules.insert("public".into(), Rule::parse("r:all"));
    assert_eq!(text(&node.handle("/git/list", &list, Some([9; 16])).unwrap()).0, 3);
    // ...but it reaches one that says nothing of reading.
    std::fs::write(tmp.path().join("repos/public/demo.git.allowed"), format!("w:{}", hex::encode(ME))).unwrap();
    assert_eq!(text(&node.handle("/git/list", &list, Some([9; 16])).unwrap()).0, 0);
    // Blocked wins over every rule.
    node.blocked.insert(ME);
    assert_eq!(text(&node.handle("/git/list", &list, Some(ME)).unwrap()).0, 1);
    node.blocked.clear();
    node.blocked.insert(NULL_IDENT);
    assert_eq!(text(&node.handle("/git/list", &list, None).unwrap()).0, 1);
}

#[test]
fn writes_are_refused_and_listing_for_push_needs_write() {
    let (_tmp, node, _) = node("r:all");
    let push = rmpv_request("public/demo.git", &[("for_push", rmpv::Value::Boolean(true))]);
    assert_eq!(text(&node.handle("/git/list", &push, Some(ME)).unwrap()), (3, "Not allowed".into()));
    for path in ["/git/push", "/git/create", "/git/delete"] {
        assert_eq!(text(&node.handle(path, &push, Some(ME)).unwrap()).0, Status::NotAllowed.to_byte(), "{path}");
    }
    assert!(node.handle("/nothing", &push, Some(ME)).is_none());
}

#[test]
fn odd_requests_get_an_answer_and_never_leave_the_folder() {
    let (tmp, node, _) = node("r:all");
    std::fs::create_dir_all(tmp.path().join("secret.git")).unwrap();
    for name in [
        "../secret.git",
        "public/../../secret.git",
        "public/demo",
        "public//demo.git",
        "/etc/passwd",
        ".git",
        "public/.git",
        "a/b/c.git",
        "",
    ] {
        let list = rmpv_request(name, &[]);
        assert_eq!(text(&node.handle("/git/list", &list, Some(ME)).unwrap()).0, 3, "{name:?}");
    }
    assert_eq!(text(&node.handle("/git/list", b"not msgpack", Some(ME)).unwrap()).0, 2);
    assert_eq!(text(&node.handle("/git/list", &[0x90], Some(ME)).unwrap()).0, 2);
    let none = rmpv_request("public/demo.git", &[]);
    assert_eq!(text(&node.handle("/git/fetch", &none, Some(ME)).unwrap()), (2, "No refs specified".into()));
    let bad = rmpv_request("public/demo.git", &[("refs", rmpv::Value::Array(vec![rmpv::Value::from("x")]))]);
    assert_eq!(text(&node.handle("/git/fetch", &bad, Some(ME)).unwrap()).0, 2);
    let name = |n: &str| {
        rmpv_request(
            "public/demo.git",
            &[("refs", rmpv::Value::Array(vec![rmpv::Value::Map(vec![(rmpv::Value::from("ref"), rmpv::Value::from(n))])]))],
        )
    };
    assert_eq!(text(&node.handle("/git/fetch", &name("refs/heads/../../x"), Some(ME)).unwrap()).0, 2);
    assert_eq!(text(&node.handle("/git/fetch", &name("refs/heads/none"), Some(ME)).unwrap()).0, 3);
}

#[test]
fn a_rules_file_that_is_too_big_cannot_grant_what_is_past_its_cap() {
    let (tmp, node, _) = node(&format!("{}r:all", "x\n".repeat(35_000)));
    let list = rmpv_request("public/demo.git", &[]);
    assert_eq!(text(&node.handle("/git/list", &list, Some(ME)).unwrap()).0, 3);
    drop(tmp);
}

fn rmpv_request(repo: &str, extra: &[(&str, rmpv::Value)]) -> Vec<u8> {
    let mut entries = vec![(rmpv::Value::from(0), rmpv::Value::from(repo))];
    entries.extend(extra.iter().map(|(k, v)| (rmpv::Value::from(*k), v.clone())));
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, &rmpv::Value::Map(entries)).unwrap();
    out
}

fn fetch_of(refs: &[&str]) -> Vec<u8> {
    let wanted = refs.iter().map(|n| rmpv::Value::Map(vec![(rmpv::Value::from("ref"), rmpv::Value::from(*n))])).collect::<Vec<_>>();
    rmpv_request("public/demo.git", &[("refs", rmpv::Value::Array(wanted))])
}

#[test]
fn a_bundle_built_lately_is_kept_until_a_ref_moves() {
    let (tmp, node, work) = node("r:all");
    let ask = fetch_of(&["refs/heads/main", "refs/tags/v1"]);
    let first = node.handle("/git/fetch", &ask, Some(ME)).unwrap();
    assert_eq!(node.cached_bundles(), 1);
    assert_eq!(node.handle("/git/fetch", &ask, Some(ME)).unwrap(), first);
    assert_eq!(node.cached_bundles(), 1);
    // The same ref pointing somewhere else is a different bundle.
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("more.txt"), "more").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "more"]);
    git(&work, &["push", "-q", tmp.path().join("repos/public/demo.git").to_str().unwrap(), "main"]);
    assert_ne!(node.handle("/git/fetch", &ask, Some(ME)).unwrap(), first);
    assert_eq!(node.cached_bundles(), 2);
    // A peer who may not read it gets nothing from the cache.
    let (_tmp, closed, _) = self::node("");
    assert_eq!(text(&closed.handle("/git/fetch", &ask, Some(ME)).unwrap()).0, 3);
}

#[test]
fn big_requests_and_big_bundles_are_refused() {
    let (_tmp, mut node, _) = node("r:all");
    let long = rmpv_request("public/demo.git", &[("pad", rmpv::Value::from("x".repeat(2 * 1024 * 1024)))]);
    assert_eq!(text(&node.handle("/git/list", &long, Some(ME)).unwrap()), (2, "Request is too large".into()));
    node.max_bundle_bytes = 100;
    let ask = fetch_of(&["refs/heads/main"]);
    assert_eq!(text(&node.handle("/git/fetch", &ask, Some(ME)).unwrap()), (0xff, "Bundle is too large".into()));
}

#[test]
fn building_stops_when_told_to() {
    let (_tmp, _node, _) = node("r:all");
    let repo = gix::open(_tmp.path().join("repos/public/demo.git")).unwrap();
    let stop = std::sync::atomic::AtomicBool::new(true);
    assert!(rngit::bundle::build_many_until(&repo, &[("refs/heads/main", None)], &[], &stop).is_err());
    let go = std::sync::atomic::AtomicBool::new(false);
    assert!(rngit::bundle::build_many_until(&repo, &[("refs/heads/main", None)], &[], &go).is_ok());
}

/// A node whose `public` group lets anyone make repositories.
fn open_node() -> (tempfile::TempDir, Node) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("repos/public")).unwrap();
    let mut node = Node::new(tmp.path().join("repos"));
    node.default_rules = Rule::parse("c:all");
    (tmp, node)
}

fn new_url() -> RepoUrl {
    "rns://00112233445566778899aabbccddeeff/public/new".parse().unwrap()
}

fn served(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().join("repos/public/new.git")
}

#[tokio::test]
async fn a_client_makes_a_repository_pushes_to_it_and_deletes_refs() {
    let (tmp, node) = open_node();
    let work = tmp.path().join("work");
    work_repo(&work, 2);
    let client = Client::new(Direct { node, peer: Some(ME), key: None });
    client.create(&new_url()).await.unwrap();
    assert_eq!(std::fs::read_to_string(tmp.path().join("repos/public/new.git.allowed")).unwrap(), format!("adm:{}", hex::encode(ME)));
    let empty = client.list(&new_url(), true).await.unwrap();
    assert!((empty.refs.is_empty(), empty.head.as_deref()) == (true, Some("refs/heads/master")));

    // A branch, then it moving forward, a tag and a second branch.
    client.push_from(&new_url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    assert_eq!(git(&served(&tmp), &["rev-parse", "main"]), git(&work, &["rev-parse", "main"]));
    std::fs::write(work.join("more.txt"), "more").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "more"]);
    client.push_from(&new_url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    git(&work, &["tag", "-a", "-m", "annotated", "v1"]);
    client.push_from(&new_url(), &work, "refs/tags/v1", "refs/tags/v1", false).await.unwrap();
    client.push_from(&new_url(), &work, "refs/heads/main", "refs/heads/feature", false).await.unwrap();
    let served_repo = served(&tmp);
    assert_eq!(git(&served_repo, &["rev-parse", "main"]), git(&work, &["rev-parse", "main"]));
    assert_eq!(git(&served_repo, &["cat-file", "-t", "v1"]), "tag");
    git(&served_repo, &["fsck", "--strict"]);

    // Cloning it back gives what was pushed.
    let dest = tmp.path().join("back.git");
    client.clone_into(&new_url(), &dest).await.unwrap();
    assert_eq!(git(&dest, &["rev-parse", "feature"]), git(&work, &["rev-parse", "main"]));

    // A branch that is no ancestor needs force.
    git(&work, &["reset", "-q", "--hard", "HEAD~1"]);
    std::fs::write(work.join("other.txt"), "other").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "diverged"]);
    let before = git(&served_repo, &["rev-parse", "main"]);
    assert!(client.push_from(&new_url(), &work, "refs/heads/main", "refs/heads/main", false).await.is_err());
    client.push_from(&new_url(), &work, "refs/heads/main", "refs/heads/main", true).await.unwrap();
    assert_ne!(git(&served_repo, &["rev-parse", "main"]), before);

    // Deleting a ref, and one that isn't there.
    client.delete_ref(&new_url(), "refs/heads/feature").await.unwrap();
    client.delete_ref(&new_url(), "refs/heads/never-was").await.unwrap();
    assert!(!git(&served_repo, &["branch", "--list", "feature"]).contains("feature"));

    // Pointing a ref at an object the repository has, and at one it hasn't.
    let main = git(&served_repo, &["rev-parse", "main"]).parse().unwrap();
    client.update_ref(&new_url(), "refs/tags/again", main, false).await.unwrap();
    assert_eq!(git(&served_repo, &["rev-parse", "again"]), main.to_string());
    let missing = "1".repeat(40).parse().unwrap();
    let error = client.update_ref(&new_url(), "refs/tags/nope", missing, false).await.unwrap_err().to_string();
    assert!(error.contains("does not exist in repository"), "{error}");
}

#[test]
fn what_a_peer_may_change_follows_the_rules() {
    let (tmp, node) = open_node();
    let make = rmpv_request("public/new.git", &[]);
    // Not telling who you are, or no create rule: no repository.
    assert_eq!(text(&node.handle("/git/create", &make, None).unwrap()), (1, "Not allowed".into()));
    assert_eq!(text(&node.handle("/git/create", &make, Some(ME)).unwrap()), (0, "\0".into()));
    assert_eq!(text(&node.handle("/git/create", &make, Some(ME)).unwrap()), (1, "Repository already exists".into()));
    assert_eq!(text(&node.handle("/git/create", &rmpv_request("nowhere/new.git", &[]), Some(ME)).unwrap()), (3, "Not found".into()));
    for odd in ["public/../x.git", "public/x", "x.git", "public/.git"] {
        assert_ne!(text(&node.handle("/git/create", &rmpv_request(odd, &[]), Some(ME)).unwrap()).0, 0, "{odd}");
    }
    assert_eq!(std::fs::read_dir(tmp.path().join("repos/public")).unwrap().count(), 2);

    // The creator may write; someone who may only read may not; someone who may do neither doesn't see it.
    std::fs::write(tmp.path().join("repos/public/new.git.allowed"), format!("adm:{}\nr:{}", hex::encode(ME), hex::encode([8; 16])))
        .unwrap();
    let delete = rmpv_request("public/new.git", &[("ref", rmpv::Value::from("refs/heads/x"))]);
    assert_eq!(text(&node.handle("/git/delete", &delete, Some(ME)).unwrap()).0, 0);
    assert_eq!(text(&node.handle("/git/delete", &delete, Some([8; 16])).unwrap()), (1, "Not allowed".into()));
    assert_eq!(text(&node.handle("/git/delete", &delete, Some([9; 16])).unwrap()), (3, "Not found".into()));
    let odd = rmpv_request("public/new.git", &[("ref", rmpv::Value::from("refs/rngit/x"))]);
    assert_eq!(text(&node.handle("/git/delete", &odd, Some(ME)).unwrap()).0, 2);
    let head = rmpv_request("public/new.git", &[("ref", rmpv::Value::from("HEAD"))]);
    assert_eq!(text(&node.handle("/git/delete", &head, Some(ME)).unwrap()).0, 2);
}

#[test]
fn the_node_refuses_a_divergent_bundle_and_leaves_the_ref() {
    let (tmp, node) = open_node();
    let work = tmp.path().join("work");
    work_repo(&work, 2);
    assert_eq!(text(&node.handle("/git/create", &rmpv_request("public/new.git", &[]), Some(ME)).unwrap()).0, 0);
    let repo = gix::open(&work).unwrap();
    let push = |refname: &str, force: bool, node: &Node| {
        let bundle = rngit::bundle::build(&repo, "refs/heads/main", None).unwrap();
        let request = rmpv_request(
            "public/new.git",
            &[
                ("local_ref", rmpv::Value::from("refs/heads/main")),
                ("remote_ref", rmpv::Value::from(refname)),
                ("force", rmpv::Value::Boolean(force)),
                ("bundle", rmpv::Value::Binary(bundle)),
            ],
        );
        text(&node.handle("/git/push", &request, Some(ME)).unwrap())
    };
    assert_eq!(push("refs/heads/main", false, &node).0, 0);
    let first = git(&served(&tmp), &["rev-parse", "main"]);
    // The same again is fine; a bundle of something unrelated isn't, unless forced.
    assert_eq!(push("refs/heads/main", false, &node).0, 0);
    git(&work, &["checkout", "-q", "--orphan", "other"]);
    std::fs::write(work.join("x.txt"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "unrelated"]);
    git(&work, &["branch", "-f", "main"]);
    let repo = gix::open(&work).unwrap();
    let push = |refname: &str, force: bool| {
        let bundle = rngit::bundle::build(&repo, "refs/heads/main", None).unwrap();
        let request = rmpv_request(
            "public/new.git",
            &[("remote_ref", rmpv::Value::from(refname)), ("force", rmpv::Value::Boolean(force)), ("bundle", rmpv::Value::Binary(bundle))],
        );
        text(&node.handle("/git/push", &request, Some(ME)).unwrap())
    };
    assert_eq!(push("refs/heads/main", false), (0xff, "Could not verify bundle".into()));
    assert_eq!(git(&served(&tmp), &["rev-parse", "main"]), first);
    // Not a branch or a tag; a bundle that isn't one.
    assert_eq!(push("refs/rngit/x", true).0, 2);
    let junk = rmpv_request(
        "public/new.git",
        &[("remote_ref", rmpv::Value::from("refs/heads/j")), ("bundle", rmpv::Value::Binary(b"junk".to_vec()))],
    );
    assert_eq!(text(&node.handle("/git/push", &junk, Some(ME)).unwrap()), (0xff, "Could not verify bundle".into()));
    assert_eq!(push("refs/heads/main", true).0, 0);
    assert_ne!(git(&served(&tmp), &["rev-parse", "main"]), first);
}

#[test]
fn quotas_stop_a_push_that_is_too_big() {
    let (tmp, mut node) = open_node();
    let work = tmp.path().join("work");
    work_repo(&work, 3);
    node.handle("/git/create", &rmpv_request("public/new.git", &[]), Some(ME)).unwrap();
    let repo = gix::open(&work).unwrap();
    let bundle = rngit::bundle::build(&repo, "refs/heads/main", None).unwrap();
    let push = |node: &Node| {
        let request = rmpv_request(
            "public/new.git",
            &[("remote_ref", rmpv::Value::from("refs/heads/main")), ("bundle", rmpv::Value::Binary(bundle.clone()))],
        );
        text(&node.handle("/git/push", &request, Some(ME)).unwrap())
    };
    let nothing = |node: &Node| assert!(node.handle("/git/list", &rmpv_request("public/new.git", &[]), Some(ME)).is_some());
    node.quotas.max_push_bytes = bundle.len() - 1;
    assert_eq!(push(&node), (2, "Bundle is too large".into()));
    node.quotas.max_push_bytes = bundle.len();
    node.quotas.max_repo_bytes = bundle.len() as u64;
    assert_eq!(push(&node), (0xff, "Repository is full".into()));
    node.quotas.max_repo_bytes = 1 << 30;
    node.quotas.pack.max_objects = 2;
    assert_eq!(push(&node), (0xff, "Could not verify bundle".into()));
    node.quotas.pack.max_objects = 1000;
    node.quotas.pack.max_total_bytes = 10;
    assert_eq!(push(&node), (0xff, "Could not verify bundle".into()));
    node.quotas.pack.max_total_bytes = 1 << 30;
    node.quotas.pack.max_object_bytes = 10;
    assert_eq!(push(&node), (0xff, "Could not verify bundle".into()));
    nothing(&node);
    assert!(git(&served(&tmp), &["for-each-ref"]).is_empty());
    node.quotas.pack.max_object_bytes = 1 << 20;
    assert_eq!(push(&node).0, 0);
    // The groups hold only so many repositories.
    node.quotas.max_repos_per_group = 1;
    assert_eq!(text(&node.handle("/git/create", &rmpv_request("public/second.git", &[]), Some(ME)).unwrap()).0, 0xff);
}

#[test]
fn a_delta_counts_as_the_object_it_makes() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(&work, &["init", "-q", "-b", "main"]);
    let big = "the same line over and over\n".repeat(8000);
    std::fs::write(work.join("big.txt"), &big).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    std::fs::write(work.join("big.txt"), format!("{big}one more\n")).unwrap();
    git(&work, &["commit", "-q", "-a", "-m", "two"]);
    let repo = gix::open(&work).unwrap();
    let bundle = rngit::bundle::build(&repo, "refs/heads/main", None).unwrap();
    let header = rngit::bundle::parse_header(&bundle).unwrap();
    let pack = &bundle[header.pack_offset..];
    let limits = |total| rngit::bundle::PackLimits { max_objects: 100, max_object_bytes: 1 << 30, max_total_bytes: total };
    let hash = repo.object_hash();
    // Two files of 224,000 bytes and the rest: the second may be stored as a small delta.
    assert!(rngit::bundle::check_pack(pack, hash, limits(1 << 30)).is_ok());
    assert!(rngit::bundle::check_pack(pack, hash, limits(300_000)).is_err());
    assert!(rngit::bundle::check_pack(&pack[..pack.len() / 2], hash, limits(1 << 30)).is_err());
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

impl rngit::sign::Signer for Key {
    fn identity_hash(&self) -> [u8; 16] {
        rngit::sign::identity_hash(&self.1)
    }
    fn public_key(&self) -> [u8; 64] {
        self.1
    }
    fn sign(&self, message: &[u8]) -> Result<[u8; 64], Error> {
        use ed25519_dalek::Signer as _;
        Ok(self.0.sign(message).to_bytes())
    }
}

async fn released(client: &Client<Direct>, tag: &str, files: &[(String, Vec<u8>)]) -> Result<(), Error> {
    client.create_release(&url(), &Key::new(5), tag, None, "Notes\nmore", files, "2026-10-26T00:00:00Z", 1_700_000_000).await
}

#[tokio::test]
async fn releases_are_made_listed_fetched_made_latest_and_deleted() {
    let (tmp, node, _) = node(&format!("r:all\nrel:{}", hex::encode(ME)));
    let me = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    let files = vec![("app.txt".to_string(), b"the app".to_vec()), ("lib.bin".to_string(), vec![1, 2, 3])];

    // Nothing yet: the list is an empty one, not an error.
    let none = me.releases(&url()).await.unwrap();
    assert!(none.releases.is_empty() && none.latest.is_none());

    released(&me, "v1", &files).await.unwrap();
    let list = me.releases(&url()).await.unwrap();
    assert_eq!(list.latest.as_deref(), Some("v1"));
    assert_eq!((list.releases[0].tag.as_str(), list.releases[0].status.as_str()), ("v1", "published"));
    // Two files, a signature each, and the manifest.
    assert_eq!(list.releases[0].artifacts, 5);
    assert_eq!(list.releases[0].created_by, hex::encode(ME));
    assert_eq!(list.releases[0].preview, "Notes\nmore");

    // It's kept as stock rngit keeps it.
    let folder = tmp.path().join("repos/public/demo.git.releases");
    let meta = std::fs::read_to_string(folder.join("v1/META")).unwrap();
    assert!(meta.contains("tag = v1") && meta.contains("status = published"), "{meta}");
    assert_eq!(std::fs::read_to_string(folder.join("latest")).unwrap(), "v1");
    assert_eq!(std::fs::read_to_string(folder.join("v1/RELEASE.md")).unwrap(), "Notes\nmore");

    // Viewed, fetched by tag and as latest, and verified against the signatures.
    let detail = me.release(&url(), "latest").await.unwrap();
    assert_eq!((detail.tag.as_str(), detail.notes.as_str()), ("v1", "Notes\nmore"));
    assert!(detail.artifacts.iter().any(|a| a.name == "lib.bin" && a.size == 3));
    assert_eq!(me.fetch_artifact(&url(), "v1", "app.txt").await.unwrap(), b"the app");
    assert_eq!(me.fetch_artifact(&url(), "latest", "lib.bin").await.unwrap(), [1, 2, 3]);
    let (manifest, got) = me.fetch_verified_release(&url(), "v1").await.unwrap();
    assert_eq!(manifest.version, "v1");
    assert_eq!(got.len(), 2);

    // A second release, then the first made latest again, then one deleted.
    released(&me, "v2", &files[..1]).await.unwrap();
    assert_eq!(me.releases(&url()).await.unwrap().latest.as_deref(), Some("v2"));
    me.set_latest_release(&url(), "v1").await.unwrap();
    assert_eq!(me.releases(&url()).await.unwrap().latest.as_deref(), Some("v1"));
    me.delete_release(&url(), "v2").await.unwrap();
    assert!(!folder.join("v2").exists());
    let gone = me.release(&url(), "v2").await.unwrap_err().to_string();
    assert!(gone.contains("not found") || gone.contains("Not found") || gone.contains("Release not found"), "{gone}");

    // Taken tags, and tags the repository doesn't have.
    assert!(released(&me, "v1", &files).await.unwrap_err().to_string().contains("already exists"));
    assert!(released(&me, "nope", &files).await.unwrap_err().to_string().contains("does not exist in repository"));
    assert!(!folder.join("nope").exists());
    assert!(me.set_latest_release(&url(), "nope").await.is_err());
}

#[tokio::test]
async fn releasing_needs_the_permission_and_everyone_identified() {
    let (_tmp, node, _) = node(&format!("r:all\nrel:{}", hex::encode(ME)));
    let me = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    released(&me, "v1", &[("a".into(), b"a".to_vec())]).await.unwrap();

    // A reader sees them; changing needs `rel`.
    let reader = Client::new(Direct { node: node.clone(), peer: Some([9; 16]), key: None });
    assert_eq!(reader.releases(&url()).await.unwrap().releases.len(), 1);
    assert_eq!(reader.fetch_artifact(&url(), "v1", "a").await.unwrap(), b"a");
    for error in [
        released(&reader, "v2", &[]).await.unwrap_err().to_string(),
        reader.delete_release(&url(), "v1").await.unwrap_err().to_string(),
        reader.set_latest_release(&url(), "v1").await.unwrap_err().to_string(),
    ] {
        assert!(error.to_lowercase().contains("not allowed"), "{error}");
    }

    // Someone who didn't identify themselves gets nothing, as stock does.
    let anonymous = Client::new(Direct { node: node.clone(), peer: None, key: None });
    assert!(anonymous.releases(&url()).await.unwrap_err().to_string().contains("Not identified"));
    // Nor does someone who can't read the repository learn it has releases.
    let (_t, closed, _) = self::node("");
    let stranger = Client::new(Direct { node: closed, peer: Some([9; 16]), key: None });
    assert!(stranger.releases(&url()).await.unwrap_err().to_string().contains("Not found"));
}

#[test]
fn a_release_is_written_only_while_it_is_a_draft_and_within_its_limits() {
    let (tmp, mut node, _) = node(&format!("adm:{}", hex::encode(ME)));
    node.quotas.max_artifact_bytes = 10;
    node.quotas.max_release_bytes = 15;
    let ask = |extra: &[(&str, rmpv::Value)]| {
        let mut all = vec![("operation", rmpv::Value::from("create"))];
        all.extend(extra.iter().cloned());
        text(&node.handle("/mgmt/release", &rmpv_request("public/demo.git", &all), Some(ME)).unwrap())
    };
    let step = |name: &str, extra: &[(&str, rmpv::Value)]| {
        let mut all = vec![("step", rmpv::Value::from(name)), ("tag", rmpv::Value::from("v1"))];
        all.extend(extra.iter().cloned());
        ask(&all)
    };
    let file = |name: &str, size: usize| {
        step("artifact", &[("artifact_name", name.into()), ("artifact_data", rmpv::Value::Binary(vec![0; size]))])
    };
    assert_eq!(step("init", &[]).0, 0);
    assert_eq!(file("one", 8).0, 0);
    // Too big for one file; too big for the release; names that leave the folder; no data.
    assert_eq!(file("two", 11), (2, "Artifact is too large".into()));
    assert_eq!(file("two", 8), (2, "Release is too large".into()));
    for name in ["../x", "a/b", "", ".hidden", "a\\b"] {
        assert_eq!(file(name, 1).0, 2, "{name}");
    }
    assert_eq!(step("artifact", &[("artifact_name", "x".into())]).0, 2);
    // Replacing a file counts what it replaces.
    assert_eq!(file("one", 10).0, 0);
    // Tags that mean a folder up are no tags.
    for tag in ["../v1", "a/b", "..", ".", ""] {
        let reply = ask(&[("step", "init".into()), ("tag", tag.into())]);
        assert_eq!(reply.0, 2, "{tag}: {reply:?}");
    }
    assert_eq!(step("finalize", &[]).0, 0);
    // Finalized: nothing more goes in, and it can't be finished twice.
    assert_eq!(file("late", 1), (1, "Release was finalized and is not writable".into()));
    assert_eq!(step("finalize", &[]).0, 1);
    assert_eq!(step("init", &[]).0, 1);
    assert_eq!(ask(&[("step", "bogus".into())]).0, 2);
    assert!(!tmp.path().join("repos/public/demo.git.releases/late").exists());
    let kept: Vec<_> = std::fs::read_dir(tmp.path().join("repos/public/demo.git.releases/v1/artifacts")).unwrap().flatten().collect();
    assert_eq!(kept.len(), 1);
}

#[test]
fn what_is_asked_of_releases_is_checked_before_anything_is_read() {
    let (_tmp, node, _) = node(&format!("adm:{}", hex::encode(ME)));
    let ask =
        |extra: &[(&str, rmpv::Value)]| text(&node.handle("/mgmt/release", &rmpv_request("public/demo.git", extra), Some(ME)).unwrap());
    assert_eq!(ask(&[]).0, 2);
    assert_eq!(ask(&[("operation", "explode".into())]).0, 2);
    // `latest` with none made; a tag that leaves the folder; an artifact that does.
    assert_eq!(ask(&[("operation", "view".into()), ("tag", "latest".into())]).0, 3);
    assert_eq!(ask(&[("operation", "view".into()), ("tag", "../x".into())]).0, 2);
    assert_eq!(ask(&[("operation", "view".into()), ("tag", "nope".into())]).0, 3);
    assert_eq!(ask(&[("operation", "fetch".into()), ("tag", "v1".into()), ("artifact", "../../x".into())]).0, 2);
    assert_eq!(ask(&[("operation", "delete".into()), ("tag", "..".into())]).0, 2);
    assert_eq!(ask(&[("operation", "list".into())]).0, 0);
    let elsewhere = node.handle("/mgmt/release", &rmpv_request("../etc/x.git", &[("operation", "list".into())]), Some(ME)).unwrap();
    assert_eq!(text(&elsewhere).0, 3);
}

/// A client that is `key`'s identity.
fn as_key(node: &Node, key: &Key) -> Client<Direct> {
    use rngit::sign::Signer;
    Client::new(Direct { node: node.clone(), peer: Some(key.identity_hash()), key: Some(key.public_key()) })
}

fn hash_of(key: &Key) -> String {
    use rngit::sign::Signer;
    hex::encode(key.identity_hash())
}

#[tokio::test]
async fn work_documents_are_written_read_commented_moved_and_deleted() {
    use rngit::client::{WorkKind, WorkScope};
    let (amy, ben) = (Key::new(1), Key::new(2));
    // Amy runs the repository; anyone may read, comment and propose.
    let (tmp, node, _) = node(&format!("r:all\ni:all\np:all\nadm:{}", hash_of(&amy)));
    let (a, b) = (as_key(&node, &amy), as_key(&node, &ben));
    let none = a.work_list(&url(), WorkScope::All).await.unwrap();
    assert!(none.active.is_empty() && none.completed.is_empty() && none.proposed.is_empty());

    // Amy writes a document; it's active, and what a reader gets verifies against her key.
    let (id, scope) = a.work_create(&url(), &amy, WorkKind::Create, "First", "The plan\n", "markdown").await.unwrap();
    assert_eq!((id, scope.as_str()), (1, "active"));
    let doc = b.work_view(&url(), 1, WorkScope::All).await.unwrap();
    assert_eq!((doc.title.as_str(), doc.content.as_str(), doc.scope.as_str()), ("First", "The plan", "active"));
    assert!(doc.verified && doc.author == hash_of(&amy), "{doc:?}");
    let list = b.work_list(&url(), WorkScope::Active).await.unwrap();
    assert_eq!((list.active.len(), list.active[0].comments, list.active[0].author.as_str()), (1, 0, hash_of(&amy).as_str()));
    // On disk as stock keeps it.
    assert!(tmp.path().join("repos/public/demo.git.work/active/1/root").is_file());

    // Ben may comment and propose, but not write one of his own.
    assert_eq!(b.work_comment(&url(), 1, WorkScope::Active, "Looks good", "markdown").await.unwrap(), 1);
    assert!(tmp.path().join("repos/public/demo.git.work/active/1/1").is_file());
    let refused = b.work_create(&url(), &ben, WorkKind::Create, "Mine", "Text", "markdown").await.unwrap_err().to_string();
    assert!(refused.to_lowercase().contains("not allowed"), "{refused}");
    let (proposed, scope) = b.work_create(&url(), &ben, WorkKind::Propose, "Idea", "Do it", "markdown").await.unwrap();
    assert_eq!((proposed, scope.as_str()), (2, "proposed"));
    let allowed = std::fs::read_to_string(tmp.path().join("repos/public/demo.git.work/2.allowed")).unwrap();
    assert_eq!(allowed, format!("i:{0}\nw:{0}\n", hash_of(&ben)));
    let viewed = a.work_view(&url(), 1, WorkScope::All).await.unwrap();
    assert_eq!(viewed.comments.len(), 1);
    assert_eq!(a.work_list(&url(), WorkScope::All).await.unwrap().active[0].comments, 1);

    // Ben edits his proposal (only its author may; Amy may not edit his words), and Amy takes it up.
    b.work_edit(&url(), &ben, 2, WorkScope::Proposed, "Idea!", "Do it now").await.unwrap();
    let edited = b.work_view(&url(), 2, WorkScope::All).await.unwrap();
    assert_eq!((edited.title.as_str(), edited.content.as_str()), ("Idea!", "Do it now"));
    assert!(edited.verified && edited.edited >= edited.created);
    let not_author = a.work_edit(&url(), &amy, 2, WorkScope::Proposed, "Mine now", "Stolen").await.unwrap_err().to_string();
    assert!(not_author.contains("not author"), "{not_author}");
    assert_eq!(a.work_activate(&url(), 2).await.unwrap(), (2, "active".into()));
    assert!(tmp.path().join("repos/public/demo.git.work/active/2/root").is_file());
    assert!(!tmp.path().join("repos/public/demo.git.work/proposed/2").exists());

    // Completed, and active again; only its author or an administrator may.
    assert_eq!(a.work_complete(&url(), 1).await.unwrap(), (1, "completed".into()));
    assert_eq!(a.work_list(&url(), WorkScope::All).await.unwrap().completed.len(), 1);
    assert_eq!(a.work_activate(&url(), 1).await.unwrap().1, "active");
    assert!(b.work_complete(&url(), 1).await.is_err());

    // Deleted: the document, its comments and its rules go.
    b.work_delete(&url(), 2, WorkScope::Active).await.unwrap_err();
    a.work_delete(&url(), 2, WorkScope::Active).await.unwrap();
    assert!(!tmp.path().join("repos/public/demo.git.work/active/2").exists());
    assert!(!tmp.path().join("repos/public/demo.git.work/2.allowed").exists());
    assert!(a.work_view(&url(), 2, WorkScope::All).await.is_err());
    // The next number is past every one used.
    let (next, _) = a.work_create(&url(), &amy, WorkKind::Create, "Third", "More", "markdown").await.unwrap();
    assert_eq!(next, 2);
}

#[tokio::test]
async fn what_is_written_has_to_be_signed_by_the_sender_and_fit() {
    use rngit::client::{WorkKind, WorkScope};
    let (amy, ben) = (Key::new(1), Key::new(2));
    let (_tmp, node, _) = node(&format!("r:all\nadm:{}", hash_of(&amy)));
    let a = as_key(&node, &amy);
    // Signed by someone else, or sent by a link whose key isn't known.
    let wrong = a.work_create(&url(), &ben, WorkKind::Create, "T", "Text", "markdown").await.unwrap_err().to_string();
    assert!(wrong.contains("Invalid signature"), "{wrong}");
    let unknown =
        Client::new(Direct { node: node.clone(), peer: Some(hex::decode(hash_of(&amy)).unwrap().try_into().unwrap()), key: None });
    assert!(unknown.work_create(&url(), &amy, WorkKind::Create, "T", "Text", "markdown").await.is_err());
    // A key that isn't the sender's.
    let borrowed = {
        use rngit::sign::Signer;
        Client::new(Direct { node: node.clone(), peer: Some(amy.identity_hash()), key: Some(ben.public_key()) })
    };
    assert!(borrowed.work_create(&url(), &ben, WorkKind::Create, "T", "Text", "markdown").await.is_err());
    // Nothing without a title or text, and nothing too long.
    assert!(
        a.work_create(&url(), &amy, WorkKind::Create, " ", "Text", "markdown").await.unwrap_err().to_string().contains("Title is required")
    );
    assert!(
        a.work_create(&url(), &amy, WorkKind::Create, "T", "  ", "markdown").await.unwrap_err().to_string().contains("Content is required")
    );
    let long = "x".repeat(256 * 1024);
    assert!(a.work_create(&url(), &amy, WorkKind::Create, "T", &long, "markdown").await.unwrap_err().to_string().contains("limit"));
    assert!(a.work_list(&url(), WorkScope::All).await.unwrap().active.is_empty());
    // Someone who didn't identify themselves is turned away.
    let anonymous = Client::new(Direct { node: node.clone(), peer: None, key: None });
    assert!(anonymous.work_list(&url(), WorkScope::All).await.unwrap_err().to_string().contains("Not identified"));
}

#[test]
fn odd_work_requests_are_answered_and_leave_nothing_behind() {
    let amy = Key::new(1);
    let (tmp, node, _) = node(&format!("r:all\nadm:{}", hash_of(&amy)));
    let sender = {
        use rngit::sign::Signer;
        Sender { hash: amy.identity_hash(), key: Some(amy.public_key()) }
    };
    let ask = |extra: &[(&str, rmpv::Value)]| {
        text(&node.handle_from("/mgmt/work", &rmpv_request("public/demo.git", extra), Some(sender)).unwrap())
    };
    assert_eq!(ask(&[]).0, 2);
    assert_eq!(ask(&[("operation", "explode".into())]).0, 1);
    // Documents by numbers that aren't, or aren't there, or that name a folder.
    for id in [rmpv::Value::from("../../x"), rmpv::Value::from(-1), rmpv::Value::Boolean(true), rmpv::Value::from(u64::MAX)] {
        let reply = ask(&[("operation", "view".into()), ("doc_id", id.clone())]);
        assert!(matches!(reply.0, 2 | 3), "{id:?}: {reply:?}");
    }
    assert_eq!(ask(&[("operation", "view".into()), ("doc_id", 1.into()), ("scope", "everywhere".into())]).0, 2);
    assert_eq!(ask(&[("operation", "view".into())]).0, 2);
    assert_eq!(ask(&[("operation", "delete".into()), ("doc_id", 5.into())]).0, 3);
    assert_eq!(ask(&[("operation", "list".into()), ("scope", "everywhere".into())]).0, 0);
    // A comment on nothing, and a document that was never signed.
    assert_eq!(ask(&[("operation", "comment".into()), ("doc_id", 5.into()), ("content", "hi".into())]).0, 3);
    assert_eq!(ask(&[("operation", "create".into()), ("title", "t".into()), ("content", "c".into())]).0, 2);
    assert!(!tmp.path().join("repos/public/demo.git.work").exists());
}

#[tokio::test]
async fn who_may_change_a_documents_rules_and_what_they_say() {
    use rngit::client::{WorkKind, WorkScope};
    let (amy, ben, cat) = (Key::new(1), Key::new(2), Key::new(3));
    let (_tmp, node, _) = node(&format!("r:all\ni:all\np:all\nadm:{}", hash_of(&amy)));
    let (a, b, c) = (as_key(&node, &amy), as_key(&node, &ben), as_key(&node, &cat));
    b.work_create(&url(), &ben, WorkKind::Propose, "Idea", "Text", "markdown").await.unwrap();

    // Amy, an administrator, reads what the proposal gives its author, and changes it.
    assert_eq!(a.work_perms(&url(), 1).await.unwrap(), format!("i:{0}\nw:{0}\n", hash_of(&ben)));
    a.set_work_perms(&url(), 1, &format!("i:{}\nw:{}\nr:none", hash_of(&ben), hash_of(&ben))).await.unwrap();
    // With reading shut for everyone else on that document, Cat no longer sees it (Ben does, and so does Amy).
    assert!(c.work_view(&url(), 1, WorkScope::All).await.is_err());
    assert!(c.work_list(&url(), WorkScope::All).await.unwrap().proposed.is_empty());
    assert!(a.work_view(&url(), 1, WorkScope::All).await.is_ok());
    // Ben is no administrator of the repository, so the rules aren't his to read.
    assert!(b.work_perms(&url(), 1).await.is_err());
    // A rule that isn't one is refused, with the line.
    let bad = a.set_work_perms(&url(), 1, "i:all\nsomething else").await.unwrap_err().to_string();
    assert!(bad.contains("Invalid permission \"something else\" on line 2"), "{bad}");
    assert!(a.set_work_perms(&url(), 1, "r:all, w:all").await.is_err());

    // The repository's and the group's, as only their administrators may.
    assert!(a.repo_perms(&url()).await.unwrap().contains("adm:"));
    a.set_repo_perms(&url(), &format!("# who may\nr:all\ni:all\np:all\nadm:{}\n", hash_of(&amy))).await.unwrap();
    assert!(b.repo_perms(&url()).await.is_err());
    assert!(b.set_repo_perms(&url(), "r:all").await.is_err());
    assert!(a.set_repo_perms(&url(), "nonsense").await.unwrap_err().to_string().contains("Invalid permission"));
    // Nobody administers the group yet.
    assert!(a.group_perms("public").await.is_err());
}

#[tokio::test]
async fn a_group_is_administered_from_the_file_beside_it() {
    let (amy, ben) = (Key::new(1), Key::new(2));
    let (tmp, node, _) = node("r:all");
    std::fs::write(tmp.path().join("repos/public.allowed"), format!("adm:{}", hash_of(&amy))).unwrap();
    let (a, b) = (as_key(&node, &amy), as_key(&node, &ben));
    assert_eq!(a.group_perms("public").await.unwrap(), format!("adm:{}", hash_of(&amy)));
    // Ben reads the repository but isn't the group's administrator.
    assert!(b.group_perms("public").await.unwrap_err().to_string().contains("Not found"));
    assert!(b.set_group_perms("public", "c:all").await.is_err());
    a.set_group_perms("public", &format!("adm:{}\nc:all\n", hash_of(&amy))).await.unwrap();
    assert_eq!(std::fs::read_to_string(tmp.path().join("repos/public.allowed")).unwrap(), format!("adm:{}\nc:all\n", hash_of(&amy)));
    // And the rule it set works: Ben may make a repository there.
    b.create(&"rns://00112233445566778899aabbccddeeff/public/new".parse().unwrap()).await.unwrap();
    // A group that isn't there, or isn't a name.
    assert!(a.group_perms("nowhere").await.is_err());
    assert!(a.group_perms("../public").await.is_err());
}

/// Fetches from `source`, a node in this process, as `peer`.
#[derive(Debug)]
struct Local {
    source: Node,
    peer: [u8; 16],
}

impl rngit::server::Upstream for Local {
    fn refs(&self, source: &RepoUrl, _timeout: std::time::Duration) -> Result<Vec<String>, String> {
        let client = Client::new(Direct { node: self.source.clone(), peer: Some(self.peer), key: None });
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
                    let listed = runtime.block_on(client.list(source, false)).map_err(|e| e.to_string())?;
                    Ok(listed.refs.into_iter().map(|(_, name)| name).collect())
                })
                .join()
                .unwrap_or_else(|_| Err("listing panicked".into()))
        })
    }

    fn pull(&self, source: &RepoUrl, dest: &std::path::Path, _timeout: std::time::Duration) -> Result<(), String> {
        let client = Client::new(Direct { node: self.source.clone(), peer: Some(self.peer), key: None });
        // On a thread of its own, as the node's callers are not in an async function.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
                    runtime.block_on(client.clone_into(source, dest)).map(drop).map_err(|e| e.to_string())
                })
                .join()
                .unwrap_or_else(|_| Err("fetching panicked".into()))
        })
    }
}

/// A node that lets anyone make repositories and write, that fetches from `source`.
fn following(source: &Node) -> (tempfile::TempDir, Node) {
    let (tmp, mut node) = open_node();
    node.default_rules = Rule::parse("c:all, r:all");
    node.upstream = Some(std::sync::Arc::new(Local { source: source.clone(), peer: ME }));
    (tmp, node)
}

fn source_url() -> String {
    url().to_string()
}

#[tokio::test]
async fn a_fork_and_a_mirror_follow_their_source() {
    let (source_tmp, source, _work) = node("r:all");
    let source_repo = source_tmp.path().join("repos/public/demo.git");
    let (tmp, node) = following(&source);
    let client = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    let fork = new_url();
    let mirror: RepoUrl = "rns://00112233445566778899aabbccddeeff/public/copy".parse().unwrap();

    client.fork(&fork, &source_url()).await.unwrap();
    client.mirror(&mirror, &source_url()).await.unwrap();
    let (forked, mirrored) = (tmp.path().join("repos/public/new.git"), tmp.path().join("repos/public/copy.git"));
    for (dir, kind) in [(&forked, "fork"), (&mirrored, "mirror")] {
        assert_eq!(git(dir, &["rev-parse", "main"]), git(&source_repo, &["rev-parse", "main"]));
        assert_eq!(git(dir, &["rev-parse", "topic"]), git(&source_repo, &["rev-parse", "topic"]));
        assert_eq!(git(dir, &["cat-file", "-t", "v2"]), "tag");
        git(dir, &["fsck", "--strict"]);
        // Marked as stock rngit marks them, which git itself reads.
        assert_eq!(git(dir, &["config", "repository.rngit.type"]), kind);
        assert_eq!(git(dir, &["config", "repository.rngit.upstream.source"]), source_url());
        assert!(git(dir, &["config", "repository.rngit.upstream.sync"]).parse::<u64>().unwrap() > 1_000_000_000);
        // Whoever made it is its administrator.
        let allowed = std::fs::read_to_string(format!("{}.allowed", dir.display())).unwrap();
        assert_eq!(allowed, format!("adm:{}", hex::encode(ME)));
    }
    assert_eq!(git(&forked, &["symbolic-ref", "HEAD"]), "refs/heads/main");

    // The source moves on: a commit on main, a new tag, and HEAD on another branch.
    let work = source_tmp.path().join("work2");
    git(source_tmp.path(), &["clone", "-q", source_repo.to_str().unwrap(), work.to_str().unwrap()]);
    std::fs::write(work.join("new.txt"), "new").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "newer"]);
    git(&work, &["tag", "v3"]);
    git(&work, &["push", "-q", "origin", "main", "v3"]);
    git(&source_repo, &["symbolic-ref", "HEAD", "refs/heads/topic"]);
    client.sync(&fork).await.unwrap();
    client.sync(&mirror).await.unwrap();
    for dir in [&forked, &mirrored] {
        assert_eq!(git(dir, &["rev-parse", "main"]), git(&source_repo, &["rev-parse", "main"]));
        assert_eq!(git(dir, &["cat-file", "-t", "v3"]), "commit");
        git(dir, &["fsck", "--strict"]);
    }
    // A mirror's HEAD follows the source's; a fork's stays where it was.
    assert_eq!(git(&mirrored, &["symbolic-ref", "HEAD"]), "refs/heads/topic");
    assert_eq!(git(&forked, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(git(&mirrored, &["config", "repository.rngit.type"]), "mirror");
}

#[tokio::test]
async fn mirrors_that_have_not_followed_lately_are_brought_up_to_date() {
    let (source_tmp, source, _work) = node("r:all");
    let source_repo = source_tmp.path().join("repos/public/demo.git");
    let (tmp, node) = following(&source);
    let client = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    let (mirror, fork) = (new_url(), "rns://00112233445566778899aabbccddeeff/public/forked".parse::<RepoUrl>().unwrap());
    client.mirror(&mirror, &source_url()).await.unwrap();
    client.fork(&fork, &source_url()).await.unwrap();
    git(&source_repo, &["branch", "later", "main"]);
    // Followed just now: nothing to do.
    assert_eq!(node.sync_mirrors(std::time::Duration::from_secs(3600)), 0);
    // Not for a day: the mirror follows, the fork is left alone.
    let (mirrored, forked) = (tmp.path().join("repos/public/new.git"), tmp.path().join("repos/public/forked.git"));
    git(&mirrored, &["config", "repository.rngit.upstream.sync", "1000"]);
    git(&forked, &["config", "repository.rngit.upstream.sync", "1000"]);
    assert_eq!(node.sync_mirrors(std::time::Duration::from_secs(24 * 3600)), 1);
    assert!(git(&mirrored, &["branch", "--list", "later"]).contains("later"));
    assert!(git(&forked, &["branch", "--list", "later"]).is_empty());
    assert!(git(&mirrored, &["config", "repository.rngit.upstream.sync"]).parse::<u64>().unwrap() > 1_000_000_000);
}

#[tokio::test]
async fn forks_and_mirrors_are_refused_what_they_should_be() {
    let (_source_tmp, source, _work) = node("r:all");
    let (tmp, node) = following(&source);
    let me = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    let reply = |result: Result<(), Error>| result.unwrap_err().to_string();
    // Sources that aren't rns:// ones, and ones that aren't sources.
    assert!(reply(me.fork(&new_url(), "https://127.0.0.1/x.git").await).contains("Prohibited source URL"));
    assert!(reply(me.fork(&new_url(), "git@example.com:x.git").await).contains("Prohibited source URL"));
    assert!(reply(me.fork(&new_url(), "rns://nope").await).contains("Invalid source URL"));
    assert!(reply(me.fork(&new_url(), "").await).contains("No source specified"));
    // A source that can't be fetched leaves nothing behind.
    let missing = "rns://00112233445566778899aabbccddeeff/public/missing";
    assert!(reply(me.fork(&new_url(), missing).await).contains("Failed to fetch from source"));
    let left: Vec<_> = std::fs::read_dir(tmp.path().join("repos/public")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(left.is_empty(), "{left:?}");
    // Who may make repositories, who has to say who they are, and what is taken.
    let anonymous = Client::new(Direct { node: node.clone(), peer: None, key: None });
    assert!(reply(anonymous.fork(&new_url(), &source_url()).await).contains("Not identified"));
    me.fork(&new_url(), &source_url()).await.unwrap();
    assert!(reply(me.fork(&new_url(), &source_url()).await).contains("already exists"));
    let (_t, mut closed) = open_node();
    closed.default_rules = Rule::parse("r:all");
    closed.upstream = node.upstream.clone();
    let reader = Client::new(Direct { node: closed, peer: Some(ME), key: None });
    assert!(reply(reader.fork(&new_url(), &source_url()).await).to_lowercase().contains("not allowed"));
    // A node with nothing to fetch with says so.
    let (_t, mut bare) = open_node();
    bare.upstream = None;
    let bare = Client::new(Direct { node: bare, peer: Some(ME), key: None });
    assert!(reply(bare.fork(&new_url(), &source_url()).await).contains("can't fetch"));

    // Syncing: needs write, and a repository that follows something.
    let plain: RepoUrl = "rns://00112233445566778899aabbccddeeff/public/plain".parse().unwrap();
    me.create(&plain).await.unwrap();
    assert!(reply(me.sync(&plain).await).contains("neither fork nor mirror"));
    let (_t, writer) = {
        let (t, mut n) = following(&source);
        n.default_rules = Rule::parse("c:all, r:all");
        (t, n)
    };
    let stranger = Client::new(Direct { node: writer, peer: Some([9; 16]), key: None });
    assert!(stranger.sync(&new_url()).await.is_err());
    assert!(reply(me.sync(&"rns://00112233445566778899aabbccddeeff/public/none".parse().unwrap()).await).contains("Not found"));
    // Syncing from a source that's gone fails, and the repository stays as it was.
    let before = git(&tmp.path().join("repos/public/new.git"), &["rev-parse", "main"]);
    git(&tmp.path().join("repos/public/new.git"), &["config", "repository.rngit.upstream.source", missing]);
    assert!(reply(me.sync(&new_url()).await).contains("sync failed"));
    assert_eq!(git(&tmp.path().join("repos/public/new.git"), &["rev-parse", "main"]), before);
}

/// Serves `repo` over smart HTTP (what an https:// source speaks), by way of git's own
/// `upload-pack`. It stops when the returned flag is set.
#[cfg(feature = "https")]
fn serve_http(repo: std::path::PathBuf) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/demo.git", listener.local_addr().unwrap());
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let Ok((stream, _)) = listener.accept() else {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let (mut length, mut chunked, mut protocol) = (0usize, false, String::new());
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end().to_string();
                if header.is_empty() {
                    break;
                }
                let lower = header.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                } else if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                    chunked = true;
                } else if lower.starts_with("git-protocol:") {
                    protocol = header["git-protocol:".len()..].trim().to_string();
                }
            }
            let mut body = Vec::new();
            if chunked {
                loop {
                    let mut size = String::new();
                    reader.read_line(&mut size).unwrap();
                    let size = usize::from_str_radix(size.trim(), 16).unwrap();
                    let mut chunk = vec![0; size + 2];
                    reader.read_exact(&mut chunk).unwrap();
                    if size == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..size]);
                }
            } else {
                body.resize(length, 0);
                reader.read_exact(&mut body).unwrap();
            }
            let advertise = line.contains("info/refs");
            let mut command = Command::new("git");
            command.arg("upload-pack").arg("--stateless-rpc");
            if advertise {
                command.arg("--advertise-refs");
            }
            let mut child = command.arg(&repo).env("GIT_PROTOCOL", &protocol).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(&body).unwrap();
            let out = child.wait_with_output().unwrap().stdout;
            let mut reply = Vec::new();
            if advertise && !protocol.contains("version=2") {
                reply.extend_from_slice(b"001e# service=git-upload-pack\n0000");
            }
            reply.extend_from_slice(&out);
            let kind = if advertise { "advertisement" } else { "result" };
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-git-upload-pack-{kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                reply.len()
            )
            .unwrap();
            stream.write_all(&reply).unwrap();
        }
    });
    (base, stop)
}

#[cfg(feature = "https")]
#[tokio::test]
async fn a_mirror_and_a_fork_can_follow_a_repository_served_over_http() {
    let (source_tmp, _source, _work) = node("r:all");
    let source_repo = source_tmp.path().join("repos/public/demo.git");
    let (source, stop) = serve_http(source_repo.clone());
    let (tmp, mut node) = open_node();
    node.default_rules = Rule::parse("c:all, r:all");
    let client = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    let (mirror, fork) = (new_url(), "rns://00112233445566778899aabbccddeeff/public/forked".parse::<RepoUrl>().unwrap());

    // A source on the node's own network, or without TLS, isn't taken unless the node is told it may.
    let refused = client.mirror(&mirror, &source).await.unwrap_err().to_string();
    assert!(refused.contains("Prohibited source URL"), "{refused}");
    assert!(!served(&tmp).exists());

    node.open_sources = true;
    let client = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    client.mirror(&mirror, &source).await.unwrap();
    client.fork(&fork, &source).await.unwrap();
    let (mirrored, forked) = (served(&tmp), tmp.path().join("repos/public/forked.git"));
    for dir in [&mirrored, &forked] {
        assert_eq!(git(dir, &["rev-parse", "main"]), git(&source_repo, &["rev-parse", "main"]));
        assert_eq!(git(dir, &["rev-parse", "topic"]), git(&source_repo, &["rev-parse", "topic"]));
        assert_eq!(git(dir, &["cat-file", "-t", "v2"]), "tag");
        git(dir, &["fsck", "--strict"]);
        assert_eq!(git(dir, &["config", "repository.rngit.upstream.source"]), source);
    }
    assert_eq!(git(&mirrored, &["symbolic-ref", "HEAD"]), git(&source_repo, &["symbolic-ref", "HEAD"]));

    // The source adds a tag and a branch, drops one, and moves HEAD.
    git(&source_repo, &["branch", "later", "main"]);
    git(&source_repo, &["tag", "v9", "main"]);
    git(&source_repo, &["branch", "-D", "topic"]);
    git(&source_repo, &["symbolic-ref", "HEAD", "refs/heads/later"]);
    client.sync(&mirror).await.unwrap();
    client.sync(&fork).await.unwrap();
    for dir in [&mirrored, &forked] {
        assert_eq!(git(dir, &["rev-parse", "later"]), git(&source_repo, &["rev-parse", "later"]));
        assert_eq!(git(dir, &["cat-file", "-t", "v9"]), "commit");
        assert!(!git(dir, &["branch", "--list", "topic"]).is_empty(), "stock keeps what the source dropped");
        git(dir, &["fsck", "--strict"]);
    }
    assert_eq!(git(&mirrored, &["symbolic-ref", "HEAD"]), "refs/heads/later");

    // A node told to drop what the source dropped does, for a mirror and a fork.
    node.prune_synced = true;
    let pruning = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    pruning.sync(&mirror).await.unwrap();
    pruning.sync(&fork).await.unwrap();
    for dir in [&mirrored, &forked] {
        assert!(git(dir, &["branch", "--list", "topic"]).is_empty(), "the branch the source dropped");
        assert!(!git(dir, &["branch", "--list", "later"]).is_empty());
    }

    // A source that stops answering fails the sync and leaves the repository as it was.
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let before = git(&mirrored, &["rev-parse", "later"]);
    assert!(client.sync(&mirror).await.unwrap_err().to_string().contains("Mirror sync failed"));
    assert_eq!(git(&mirrored, &["rev-parse", "later"]), before);
}

#[tokio::test]
async fn sources_that_are_not_rns_or_https_are_refused() {
    let (_tmp, node) = open_node();
    let client = Client::new(Direct { node, peer: Some(ME), key: None });
    for source in [
        "ssh://git@example.com/a/b.git",
        "git@example.com:a/b.git",
        "file:///etc",
        "/etc",
        "https://user:pw@8.8.8.8/a.git",
        "https://127.0.0.1/a.git",
        "https://[::1]/a.git",
    ] {
        let refused = client.mirror(&new_url(), source).await.unwrap_err().to_string();
        assert!(refused.contains("Prohibited source URL") || refused.contains("Invalid source URL"), "{source}: {refused}");
    }
}

#[cfg(feature = "https")]
#[tokio::test]
async fn a_source_that_is_larger_than_a_repository_may_be_is_not_kept() {
    let (source_tmp, _source, _work) = node("r:all");
    let (source, stop) = serve_http(source_tmp.path().join("repos/public/demo.git"));
    let (tmp, mut node) = open_node();
    node.default_rules = Rule::parse("c:all, r:all");
    node.open_sources = true;
    node.quotas.max_repo_bytes = 1;
    let client = Client::new(Direct { node, peer: Some(ME), key: None });
    let refused = client.mirror(&new_url(), &source).await.unwrap_err().to_string();
    assert!(refused.contains("Failed to fetch from source"), "{refused}");
    assert!(!served(&tmp).exists());
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path().join("repos/public")).unwrap().flatten().collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[tokio::test]
async fn a_node_drops_what_an_rns_source_dropped_only_when_told_to() {
    let (source_tmp, source, _work) = node("r:all");
    let source_repo = source_tmp.path().join("repos/public/demo.git");
    let (tmp, mut node) = following(&source);
    let client = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    let mirror = new_url();
    client.mirror(&mirror, &source_url()).await.unwrap();
    let dir = tmp.path().join("repos/public/new.git");
    git(&source_repo, &["branch", "-D", "topic"]);
    git(&source_repo, &["tag", "-d", "v1"]);
    client.sync(&mirror).await.unwrap();
    assert!(!git(&dir, &["branch", "--list", "topic"]).is_empty());
    assert!(!git(&dir, &["tag", "--list", "v1"]).is_empty());

    node.prune_synced = true;
    let pruning = Client::new(Direct { node: node.clone(), peer: Some(ME), key: None });
    pruning.sync(&mirror).await.unwrap();
    assert!(git(&dir, &["branch", "--list", "topic"]).is_empty());
    assert!(git(&dir, &["tag", "--list", "v1"]).is_empty());
    assert!(!git(&dir, &["tag", "--list", "v2"]).is_empty() && !git(&dir, &["branch", "--list", "main"]).is_empty());
    git(&dir, &["fsck", "--strict"]);
}

#[tokio::test]
async fn the_owner_manages_what_the_node_hosts_whatever_the_rules() {
    let (source_tmp, source, _work) = node("r:all");
    let source_repo = source_tmp.path().join("repos/public/demo.git");
    let (tmp, mut node) = following(&source);
    // Nobody may make anything; the owner still can.
    node.default_rules = Rule::parse("r:all");
    let repos = tmp.path().join("repos");

    node.make_repository("team", "empty.git", None).unwrap();
    assert_eq!(git(&repos.join("team/empty.git"), &["symbolic-ref", "HEAD"]), "refs/heads/master");
    assert!(!repos.join("team/empty.git.allowed").exists());
    node.follow_source("mirrors", "demo.git", &format!(" {} ", source_url()), Follows::Mirror, Some(&ME)).unwrap();
    let mirrored = repos.join("mirrors/demo.git");
    assert_eq!(git(&mirrored, &["rev-parse", "main"]), git(&source_repo, &["rev-parse", "main"]));
    assert_eq!(std::fs::read_to_string(repos.join("mirrors/demo.git.allowed")).unwrap(), format!("adm:{}", hex::encode(ME)));

    assert_eq!(node.groups(), ["mirrors", "public", "team"]);
    let hosted = node.hosted();
    let names: Vec<_> = hosted.iter().map(|h| format!("{}/{}", h.group, h.name)).collect();
    assert_eq!(names, ["mirrors/demo.git", "team/empty.git"]);
    let following = hosted[0].following.as_ref().unwrap();
    assert_eq!((following.kind, following.source.as_str()), (Follows::Mirror, source_url().as_str()));
    assert!(following.synced > 1_000_000_000 && hosted[0].bytes > 0 && hosted[1].following.is_none());

    // Syncing follows the source; a repository that follows nothing can't.
    git(&source_repo, &["branch", "later", "main"]);
    node.sync_repository("mirrors", "demo.git").unwrap();
    assert!(git(&mirrored, &["branch", "--list", "later"]).contains("later"));
    assert!(node.sync_repository("team", "empty.git").unwrap_err().contains("neither"));

    // Rules are checked as a request's are; empty removes them.
    node.set_rules("team", Some("empty.git"), "r:all\nadm:none").unwrap();
    assert_eq!(node.rules("team", Some("empty.git")).unwrap(), "r:all\nadm:none");
    assert!(node.set_rules("team", Some("empty.git"), "r:all, w:all").unwrap_err().contains("line 1"));
    assert!(node.set_rules("team", Some("gone.git"), "r:all").is_err());
    node.set_rules("team", None, "c:all").unwrap();
    assert_eq!(std::fs::read_to_string(repos.join("team.allowed")).unwrap(), "c:all");
    node.set_rules("team", None, " ").unwrap();
    assert!(!repos.join("team.allowed").exists() && node.rules("team", None).unwrap().is_empty());

    // Renaming takes what sits beside it along; it never overwrites.
    std::fs::create_dir_all(repos.join("mirrors/demo.git.releases/v1")).unwrap();
    assert!(node.rename_repository("mirrors", "demo.git", "team", "empty.git").unwrap_err().contains("already there"));
    node.rename_repository("mirrors", "demo.git", "kept", "demo2.git").unwrap();
    assert!(repos.join("kept/demo2.git.releases/v1").is_dir() && repos.join("kept/demo2.git.allowed").is_file());
    assert!(!repos.join("mirrors/demo.git").exists() && !repos.join("mirrors/demo.git.allowed").exists());
    node.sync_repository("kept", "demo2.git").unwrap();

    // Deleting takes them too.
    node.delete_repository("kept", "demo2.git").unwrap();
    assert_eq!(std::fs::read_dir(repos.join("kept")).unwrap().count(), 0);
    assert!(node.delete_repository("kept", "demo2.git").is_err());

    // Names that aren't plain, and sources it doesn't take, are refused before anything is made.
    for (group, repo) in [("..", "x.git"), ("team", "../x.git"), ("team", "x"), (".hidden", "x.git")] {
        assert!(node.make_repository(group, repo, None).is_err(), "{group}/{repo}");
        assert!(node.delete_repository(group, repo).is_err(), "{group}/{repo}");
    }
    assert!(node.make_group("a/b").is_err());
    assert!(node.follow_source("team", "ssh.git", "ssh://example.com/x.git", Follows::Fork, None).unwrap_err().contains("Prohibited"));
    assert!(node.follow_source("team", "none.git", "", Follows::Fork, None).is_err());
    assert!(!repos.join("team/ssh.git").exists());
}
