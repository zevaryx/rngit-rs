mod common;

use common::*;
use rngit::error::{Error, Status};
use rngit::server::{NULL_IDENT, Node, Rule, Who};
use rngit::{Client, RepoUrl, Response, Transport};

const ME: [u8; 16] = [7; 16];

/// A client's view of a node in this process: requests go straight to it.
struct Direct {
    node: Node,
    peer: Option<[u8; 16]>,
}

impl Transport for Direct {
    async fn request(&self, path: &str, data: Vec<u8>) -> Result<Response, Error> {
        self.node.handle(path, &data, self.peer).ok_or_else(|| Error::Transport("unknown path".into()))
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
    let client = Client::new(Direct { node, peer: Some(ME) });
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
    let (_tmp, mut node, _) = node(&format!("r:{}", hex::encode(ME)));
    let list = rmpv_request("public/demo.git", &[]);
    // The named identity reads, another one and an unidentified peer see nothing.
    assert_eq!(text(&node.handle("/git/list", &list, Some(ME)).unwrap()).0, 0);
    assert_eq!(text(&node.handle("/git/list", &list, Some([9; 16])).unwrap()), (3, "Not found".into()));
    assert_eq!(text(&node.handle("/git/list", &list, None).unwrap()).0, 3);
    // A group rule from the node's settings reaches repositories without their own rule.
    node.group_rules.insert("public".into(), Rule::parse("r:all"));
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
    for path in ["/git/push", "/git/create", "/git/delete", "/mgmt/perms", "/mgmt/work"] {
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
    let client = Client::new(Direct { node, peer: Some(ME) });
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
    let me = Client::new(Direct { node: node.clone(), peer: Some(ME) });
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
    let me = Client::new(Direct { node: node.clone(), peer: Some(ME) });
    released(&me, "v1", &[("a".into(), b"a".to_vec())]).await.unwrap();

    // A reader sees them; changing needs `rel`.
    let reader = Client::new(Direct { node: node.clone(), peer: Some([9; 16]) });
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
    let anonymous = Client::new(Direct { node: node.clone(), peer: None });
    assert!(anonymous.releases(&url()).await.unwrap_err().to_string().contains("Not identified"));
    // Nor does someone who can't read the repository learn it has releases.
    let (_t, closed, _) = self::node("");
    let stranger = Client::new(Direct { node: closed, peer: Some([9; 16]) });
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
