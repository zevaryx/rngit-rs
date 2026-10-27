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

fn text(response: &Response) -> (u8, String) {
    match response {
        Response::Packet(p) => (p[0], String::from_utf8_lossy(&p[1..]).into_owned()),
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
