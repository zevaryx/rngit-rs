mod common;

use common::*;
use rngit::bundle;
use rngit::{Client, Error, RepoUrl, Status};

fn url() -> RepoUrl {
    "rns://727ca184e3d7e83e0e35146940920442/public/demo".parse().unwrap()
}

fn remote_sha(dir: &std::path::Path, name: &str) -> String {
    git(dir, &["rev-parse", name])
}

#[tokio::test]
async fn lists_refs_and_head() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 2);
    git(&work, &["tag", "v1.0.0"]);
    bare_repo(&server);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main", "v1.0.0"]);

    let client = Client::new(MockNode::new(server.clone()));
    let refs = client.list(&url(), false).await.unwrap();
    let main = remote_sha(&work, "main");
    assert_eq!(refs.head.as_deref(), Some("refs/heads/main"));
    assert_eq!(refs.get("refs/heads/main").unwrap().to_string(), main);
    assert_eq!(refs.get("refs/tags/v1.0.0").unwrap().to_string(), main);
}

#[tokio::test]
async fn clone_then_fsck_and_log_are_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server, dest) = (tmp.path().join("work"), tmp.path().join("server.git"), tmp.path().join("dest.git"));
    work_repo(&work, 5);
    git(&work, &["tag", "v1.0.0"]);
    bare_repo(&server);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main", "v1.0.0"]);

    let client = Client::new(MockNode::new(server));
    client.clone_into(&url(), &dest).await.unwrap();

    git(&dest, &["fsck", "--strict"]);
    assert_eq!(git(&dest, &["rev-list", "--count", "main"]), "5");
    assert_eq!(remote_sha(&dest, "main"), remote_sha(&work, "main"));
    assert_eq!(git(&dest, &["symbolic-ref", "HEAD"]), "refs/heads/main");
}

#[tokio::test]
async fn clone_of_empty_repo_makes_an_empty_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let (server, dest) = (tmp.path().join("server.git"), tmp.path().join("dest.git"));
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    let remote = client.clone_into(&url(), &dest).await.unwrap();
    assert!(remote.refs.is_empty());
    assert!(gix::open(&dest).is_ok());
}

#[tokio::test]
async fn push_new_branch_then_fast_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 2);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server.clone()));

    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    git(&server, &["fsck", "--strict"]);
    assert_eq!(remote_sha(&server, "main"), remote_sha(&work, "main"));

    // Add commits and push again: this one carries a prerequisite.
    std::fs::write(work.join("more.txt"), "more\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "three"]);
    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    git(&server, &["fsck", "--strict"]);
    assert_eq!(remote_sha(&server, "main"), remote_sha(&work, "main"));
    assert_eq!(git(&server, &["rev-list", "--count", "main"]), "3");
}

#[tokio::test]
async fn push_tag_and_other_branch_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 1);
    git(&work, &["tag", "v1.0.0"]);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server.clone()));
    client.push_from(&url(), &work, "refs/tags/v1.0.0", "refs/tags/v1.0.0", false).await.unwrap();
    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/feature", false).await.unwrap();
    assert_eq!(remote_sha(&server, "refs/tags/v1.0.0"), remote_sha(&work, "main"));
    assert_eq!(remote_sha(&server, "refs/heads/feature"), remote_sha(&work, "main"));
}

#[tokio::test]
async fn non_fast_forward_is_refused_locally_unless_forced() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, other, server) = (tmp.path().join("work"), tmp.path().join("other"), tmp.path().join("server.git"));
    work_repo(&work, 2);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server.clone()));
    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();

    // A second history that doesn't contain the server's tip.
    work_repo(&other, 3);
    let before = client.transport().requests().len();
    let err = client.push_from(&url(), &other, "refs/heads/main", "refs/heads/main", false).await.unwrap_err();
    assert!(matches!(err, Error::NonFastForward), "{err:?}");
    // Only the list request went out.
    assert_eq!(client.transport().requests().len(), before + 1);

    client.push_from(&url(), &other, "refs/heads/main", "refs/heads/main", true).await.unwrap();
    assert_eq!(remote_sha(&server, "main"), remote_sha(&other, "main"));
    git(&server, &["fsck", "--strict"]);
}

#[tokio::test]
async fn rewound_local_branch_is_not_a_fast_forward() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 3);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    git(&work, &["reset", "-q", "--hard", "HEAD~1"]);
    let err = client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap_err();
    assert!(matches!(err, Error::NonFastForward));
}

#[tokio::test]
async fn pushing_without_permission_surfaces_the_status() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 1);
    bare_repo(&server);
    let mut node = MockNode::new(server);
    node.allow_write = false;
    let client = Client::new(node);
    match client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await {
        Err(Error::Remote { status: Status::NotAllowed, message }) => assert_eq!(message, "Not allowed"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn short_ref_names_are_rejected_before_sending() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    let err = client.push_bundle(&url(), "HEAD", "refs/heads/main", false, Vec::new()).await.unwrap_err();
    assert!(matches!(err, Error::InvalidInput(_)));
    assert!(client.transport().requests().is_empty());
}

#[tokio::test]
async fn unknown_repo_is_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    let missing: RepoUrl = "rns://727ca184e3d7e83e0e35146940920442/public/nope".parse().unwrap();
    assert!(matches!(client.list(&missing, false).await, Err(Error::Remote { status: Status::NotFound, .. })));
}

#[tokio::test]
async fn fetch_with_no_wants_never_asks_the_node() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    assert!(matches!(client.fetch_bundle(&url(), &[]).await, Err(Error::InvalidInput(_))));
}

#[tokio::test]
async fn oversize_bundle_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 3);
    bare_repo(&server);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main"]);
    let limits = rngit::Limits { max_bundle_bytes: 100, ..Default::default() };
    let client = Client::new(MockNode::new(server)).with_limits(limits);
    let err = client.clone_into(&url(), &tmp.path().join("dest.git")).await.unwrap_err();
    assert!(matches!(err, Error::TooLarge { what: "bundle", .. }), "{err:?}");
}

#[tokio::test]
async fn create_fork_mirror_sync_send_the_right_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    let target: RepoUrl = "rns://727ca184e3d7e83e0e35146940920442/public/new".parse().unwrap();
    client.create(&target).await.unwrap();
    client.fork(&url(), "rns://727ca184e3d7e83e0e35146940920442/public/other").await.unwrap();
    client.mirror(&url(), "https://example.com/x.git").await.unwrap();
    client.sync(&url()).await.unwrap();
    let log = client.transport().requests();
    let paths: Vec<_> = log.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["/git/create", "/git/fork", "/git/mirror", "/git/sync"]);
    assert_eq!(field(&log[0].1, "0"), None); // key 0 is an integer, not the string "0"
    assert_eq!(field(&log[1].1, "source").and_then(|v| v.as_str()), Some("rns://727ca184e3d7e83e0e35146940920442/public/other"));
}

#[test]
fn ingest_rejects_refs_that_escape_refs_namespace() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = bundle::open_or_init_bare(&tmp.path().join("r.git")).unwrap();
    let sha = "0".repeat(40);
    for name in ["HEAD", "../evil", "refs/heads/a..b", "config"] {
        let data = format!("# v2 git bundle\n{sha} {name}\n\n");
        assert!(bundle::ingest(&repo, data.as_bytes()).is_err(), "{name}");
    }
}

#[test]
fn ingest_rejects_ref_whose_object_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = bundle::open_or_init_bare(&tmp.path().join("r.git")).unwrap();
    let data = format!("# v2 git bundle\n{} refs/heads/main\n\n", "1".repeat(40));
    assert!(bundle::ingest(&repo, data.as_bytes()).is_err());
}

#[test]
fn parse_header_survives_garbage() {
    for bad in [
        &b""[..],
        b"# v2 git bundle",
        b"# v3 git bundle\n\n",
        b"# v2 git bundle\nnot a ref line\n\n",
        b"# v2 git bundle\nzz refs/heads/x\n\n",
        b"\xff\xfe\n",
    ] {
        assert!(bundle::parse_header(bad).is_err(), "{bad:?}");
    }
    let ok = bundle::parse_header(b"# v2 git bundle\n\n").unwrap();
    assert!(ok.refs.is_empty() && ok.prerequisites.is_empty());
}

#[test]
fn ingests_bundle_captured_from_stock_rngit() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = bundle::open_or_init_bare(&tmp.path().join("r.git")).unwrap();
    let data = include_bytes!("fixtures/fetch-bundle.bin");
    let refs = bundle::ingest(&repo, data).unwrap();
    assert!(!refs.is_empty());
    git(&tmp.path().join("r.git"), &["fsck", "--strict"]);
}

#[tokio::test]
async fn second_fetch_sends_haves_and_gets_a_thin_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server, dest) = (tmp.path().join("work"), tmp.path().join("server.git"), tmp.path().join("dest.git"));
    work_repo(&work, 3);
    bare_repo(&server);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main"]);
    let client = Client::new(MockNode::new(server.clone()));
    client.clone_into(&url(), &dest).await.unwrap();

    std::fs::write(work.join("new.txt"), "new\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "four"]);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main"]);
    let remote = client.list(&url(), false).await.unwrap();
    let wants: Vec<rngit::Want> = remote.refs.iter().map(|(sha, name)| rngit::Want { sha: *sha, name: name.clone() }).collect();
    client.fetch_into(&url(), &wants, &dest).await.unwrap();

    let request = client.transport().requests().into_iter().rfind(|(p, _)| p == "/git/fetch").unwrap().1;
    let refs = common::field(&request, "refs").unwrap().as_array().unwrap();
    let have = rngit::wire::MapRef::new(&refs[0]).unwrap().str("have").expect("per-ref have");
    assert_eq!(have.len(), 40);
    assert!(common::field(&request, "have").is_some(), "other tips are listed too");
    git(&dest, &["fsck", "--strict"]);
    assert_eq!(remote_sha(&dest, "main"), remote_sha(&work, "main"));
    assert_eq!(git(&dest, &["rev-list", "--count", "main"]), "4");
}

#[tokio::test]
async fn a_bundle_whose_prerequisite_we_lack_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, empty) = (tmp.path().join("work"), tmp.path().join("empty.git"));
    work_repo(&work, 2);
    let repo = gix::open(&work).unwrap();
    let first = bundle::resolve_ref(&repo, "refs/heads/main").unwrap();
    std::fs::write(work.join("x.txt"), "x\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "three"]);
    let thin = bundle::build(&gix::open(&work).unwrap(), "refs/heads/main", Some(first)).unwrap();
    let target = bundle::open_or_init_bare(&empty).unwrap();
    let err = bundle::ingest(&target, &thin).unwrap_err();
    assert!(matches!(err, Error::Protocol(ref m) if m.contains("which we don't have")), "{err:?}");
}

#[tokio::test]
async fn delete_ref_and_update_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 2);
    bare_repo(&server);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main"]);
    let client = Client::new(MockNode::new(server.clone()));
    let tip = gix::ObjectId::from_hex(remote_sha(&server, "main").as_bytes()).unwrap();

    client.update_ref(&url(), "refs/tags/v1", tip, false).await.unwrap();
    assert_eq!(remote_sha(&server, "refs/tags/v1"), tip.to_string());
    let missing = gix::ObjectId::from_hex(b"0000000000000000000000000000000000000001").unwrap();
    let err = client.update_ref(&url(), "refs/tags/v2", missing, false).await.unwrap_err();
    assert!(matches!(err, Error::Remote { status: Status::ServerError, .. }), "{err:?}");

    client.delete_ref(&url(), "refs/tags/v1").await.unwrap();
    assert!(gix::open(&server).unwrap().find_reference("refs/tags/v1").is_err());
    // A ref that isn't there is fine; names that aren't full refs never leave the client.
    client.delete_ref(&url(), "refs/tags/never-existed").await.unwrap();
    let before = client.transport().requests().len();
    for bad in ["main", "heads/main", "refs/../x", ""] {
        assert!(matches!(client.delete_ref(&url(), bad).await, Err(Error::InvalidInput(_))), "{bad}");
        assert!(matches!(client.update_ref(&url(), bad, tip, false).await, Err(Error::InvalidInput(_))), "{bad}");
    }
    assert_eq!(client.transport().requests().len(), before);
}

#[tokio::test]
async fn annotated_tag_keeps_its_tag_object() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server, dest) = (tmp.path().join("work"), tmp.path().join("server.git"), tmp.path().join("dest.git"));
    work_repo(&work, 2);
    git(&work, &["tag", "-a", "-m", "release notes", "v2.0"]);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server.clone()));
    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    client.push_from(&url(), &work, "refs/tags/v2.0", "refs/tags/v2.0", false).await.unwrap();
    assert_eq!(git(&server, &["cat-file", "-t", "refs/tags/v2.0"]), "tag");
    assert_eq!(git(&server, &["rev-parse", "refs/tags/v2.0"]), git(&work, &["rev-parse", "refs/tags/v2.0"]));
    git(&server, &["fsck", "--strict"]);

    // And it comes back as a tag object too.
    let sha = gix::ObjectId::from_hex(remote_sha(&work, "refs/tags/v2.0").as_bytes()).unwrap();
    client.fetch_into(&url(), &[rngit::Want { sha, name: "refs/tags/v2.0".into() }], &dest).await.unwrap();
    assert_eq!(git(&dest, &["cat-file", "-t", "refs/tags/v2.0"]), "tag");
    git(&dest, &["fsck", "--strict"]);
}

#[tokio::test]
async fn pushing_an_object_the_node_has_moves_the_ref_without_a_bundle() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 2);
    git(&work, &["tag", "light"]);
    git(&work, &["branch", "other"]);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server.clone()));
    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    let before = client.transport().requests().len();

    // Both already point at what the node holds.
    client.push_from(&url(), &work, "refs/tags/light", "refs/tags/light", false).await.unwrap();
    client.push_from(&url(), &work, "refs/heads/other", "refs/heads/other", false).await.unwrap();
    let sent: Vec<_> = client.transport().requests().split_off(before).into_iter().filter(|(p, _)| p == "/git/push").collect();
    assert_eq!(sent.len(), 2);
    for (_, request) in &sent {
        assert!(common::field(request, "operations").is_some() && common::field(request, "bundle").is_none(), "{request:?}");
    }
    assert_eq!(remote_sha(&server, "refs/tags/light"), remote_sha(&work, "main"));
    assert_eq!(remote_sha(&server, "refs/heads/other"), remote_sha(&work, "main"));
    git(&server, &["fsck", "--strict"]);

    // A batch with a ref list in hand asks the node for it zero more times.
    let remote = client.list(&url(), true).await.unwrap();
    let before = client.transport().requests().len();
    client.push_from_with(&url(), &work, "refs/heads/main", "refs/heads/third", false, &remote).await.unwrap();
    let lists = client.transport().requests().split_off(before).into_iter().filter(|(p, _)| p == "/git/list").count();
    assert_eq!(lists, 0);
}

#[tokio::test]
async fn an_older_commit_the_node_has_in_its_history_is_not_sent_again() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 3);
    git(&work, &["branch", "older", "HEAD~1"]);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server.clone()));
    client.push_from(&url(), &work, "refs/heads/main", "refs/heads/main", false).await.unwrap();
    let before = client.transport().requests().len();
    client.push_from(&url(), &work, "refs/heads/older", "refs/heads/older", false).await.unwrap();
    let sent: Vec<_> = client.transport().requests().split_off(before).into_iter().filter(|(p, _)| p == "/git/push").collect();
    assert!(common::field(&sent[0].1, "operations").is_some(), "{:?}", sent[0].1);
    assert_eq!(remote_sha(&server, "refs/heads/older"), remote_sha(&work, "HEAD~1"));

    // Objects the node lacks still travel as a bundle.
    git(&work, &["checkout", "-q", "-b", "new-work"]);
    std::fs::write(work.join("n.txt"), "n\n").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "new"]);
    let before = client.transport().requests().len();
    client.push_from(&url(), &work, "refs/heads/new-work", "refs/heads/new-work", false).await.unwrap();
    let sent: Vec<_> = client.transport().requests().split_off(before).into_iter().filter(|(p, _)| p == "/git/push").collect();
    assert!(common::field(&sent[0].1, "bundle").is_some());
    git(&server, &["fsck", "--strict"]);
}
