mod common;

use common::*;
use rngit::Error;
use rngit::local::{self, EntryKind};

/// A bare copy of a repository with folders, a binary file, a long text and a link.
fn copy() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let (work, copy) = (tmp.path().join("work"), tmp.path().join("copy.git"));
    work_repo(&work, 1);
    std::fs::create_dir_all(work.join("src/deep")).unwrap();
    std::fs::write(work.join("README.md"), "# Hello\nwith ünïcode\n").unwrap();
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(work.join("src/deep/x.txt"), "x").unwrap();
    std::fs::write(work.join("blob.bin"), [0u8, 1, 2, 255, 254]).unwrap();
    std::fs::write(work.join("long.txt"), "é".repeat(100)).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("README.md", work.join("link")).unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "second commit\n\nwith a body"]);
    git(&work, &["tag", "v1"]);
    git(&work, &["tag", "-a", "-m", "annotated", "v2"]);
    git(&work, &["checkout", "-q", "-b", "topic"]);
    std::fs::write(work.join("topic.txt"), "t").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "on topic"]);
    bare_repo(&copy);
    git(&work, &["push", "-q", copy.to_str().unwrap(), "main", "topic", "--tags"]);
    (tmp, copy)
}

#[test]
fn the_log_is_newest_first_and_capped() {
    let (_tmp, copy) = copy();
    let log = local::log(&copy, "refs/heads/topic", 10).unwrap();
    let subjects: Vec<&str> = log.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, ["on topic", "second commit", "commit 0"]);
    assert!(log[0].time > 0 && !log[0].author.is_empty());
    assert_eq!(log[0].id.to_string(), git(&copy, &["rev-parse", "topic"]));
    assert_eq!(local::log(&copy, "refs/heads/topic", 2).unwrap().len(), 2);
    assert!(local::log(&copy, "refs/heads/topic", 0).unwrap().is_empty());
    // A tag, annotated or not, is followed to its commit.
    assert_eq!(local::log(&copy, "refs/tags/v2", 5).unwrap()[0].subject, "second commit");
    assert_eq!(local::log(&copy, "refs/tags/v1", 5).unwrap()[0].subject, "second commit");
}

#[test]
fn names_that_are_not_refs_are_refused_or_not_found() {
    let (_tmp, copy) = copy();
    assert!(matches!(local::log(&copy, "main", 5), Err(Error::InvalidInput(_))));
    assert!(matches!(local::log(&copy, "refs/heads/nope", 5), Err(Error::Git(_))));
    assert!(local::log(&copy.join("nowhere"), "refs/heads/main", 5).is_err());
}

#[test]
fn a_folder_lists_folders_first_with_sizes() {
    let (_tmp, copy) = copy();
    let top = local::tree(&copy, "refs/heads/main", "").unwrap();
    let names: Vec<(&str, EntryKind)> = top.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(names[0], ("src", EntryKind::Dir));
    for file in ["README.md", "blob.bin", "file0.txt", "long.txt"] {
        assert!(names.contains(&(file, EntryKind::File)), "{names:?}");
    }
    #[cfg(unix)]
    assert!(names.contains(&("link", EntryKind::Link)));
    assert!(top.iter().all(|e| e.name != "topic.txt"), "only what the branch has");
    assert_eq!(top.iter().find(|e| e.name == "blob.bin").unwrap().size, Some(5));
    assert_eq!(top.iter().find(|e| e.name == "src").unwrap().size, None);
    let sub = local::tree(&copy, "refs/heads/main", "src").unwrap();
    assert_eq!(sub.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["deep", "main.rs"]);
    assert_eq!(local::tree(&copy, "refs/heads/main", "src/deep").unwrap()[0].name, "x.txt");
    assert!(local::tree(&copy, "refs/heads/topic", "").unwrap().iter().any(|e| e.name == "topic.txt"));
}

#[test]
fn paths_that_leave_the_tree_or_are_not_folders_are_refused() {
    let (_tmp, copy) = copy();
    for bad in ["..", "src/..", "../src", "/src", "src/", "src//deep", ".", "src/./deep", "a\0b"] {
        assert!(matches!(local::tree(&copy, "refs/heads/main", bad), Err(Error::InvalidInput(_))), "{bad:?}");
        assert!(matches!(local::file(&copy, "refs/heads/main", bad, 100), Err(Error::InvalidInput(_))), "{bad:?}");
    }
    assert!(matches!(local::tree(&copy, "refs/heads/main", "README.md"), Err(Error::InvalidInput(_))));
    assert!(matches!(local::tree(&copy, "refs/heads/main", "missing"), Err(Error::Git(_))));
    assert!(matches!(local::file(&copy, "refs/heads/main", "src", 100), Err(Error::InvalidInput(_))));
    assert!(matches!(local::file(&copy, "refs/heads/main", "", 100), Err(Error::InvalidInput(_))));
    let long = "a/".repeat(2000);
    assert!(matches!(local::tree(&copy, "refs/heads/main", &long), Err(Error::InvalidInput(_))));
}

#[test]
fn a_file_is_text_cut_at_the_limit_or_binary() {
    let (_tmp, copy) = copy();
    let readme = local::file(&copy, "refs/heads/main", "README.md", 1000).unwrap();
    assert_eq!((readme.text.as_deref(), readme.truncated, readme.size), (Some("# Hello\nwith ünïcode\n"), false, 23));
    let nested = local::file(&copy, "refs/heads/main", "src/main.rs", 1000).unwrap();
    assert_eq!(nested.text.as_deref(), Some("fn main() {}\n"));
    // Cut in the middle of a two-byte character: still text, and says it is cut.
    let cut = local::file(&copy, "refs/heads/main", "long.txt", 101).unwrap();
    assert_eq!((cut.truncated, cut.size), (true, 200));
    assert_eq!(cut.text.unwrap(), "é".repeat(50));
    let binary = local::file(&copy, "refs/heads/main", "blob.bin", 1000).unwrap();
    assert_eq!((binary.text, binary.truncated, binary.size), (None, false, 5));
    assert!(matches!(local::file(&copy, "refs/heads/main", "nope.txt", 10), Err(Error::Git(_))));
}

#[tokio::test]
async fn a_mirrored_repository_can_be_read_offline() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server, dest) = (tmp.path().join("work"), tmp.path().join("server.git"), tmp.path().join("dest.git"));
    work_repo(&work, 3);
    bare_repo(&server);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main"]);
    let url: rngit::RepoUrl = "rns://727ca184e3d7e83e0e35146940920442/public/demo".parse().unwrap();
    let client = rngit::Client::new(MockNode::new(server));
    client.mirror_into(&url, &dest).await.unwrap();
    let log = local::log(&dest, "refs/heads/main", 10).unwrap();
    assert_eq!(log.len(), 3);
    assert_eq!(local::tree(&dest, "refs/heads/main", "").unwrap().len(), 3);
}

#[test]
fn the_refs_of_a_copy_are_listed_with_its_head() {
    let (_tmp, copy) = copy();
    let refs = local::refs(&copy).unwrap();
    let mut names: Vec<&str> = refs.refs.iter().map(|(_, n)| n.as_str()).collect();
    names.sort();
    assert_eq!(names, ["refs/heads/main", "refs/heads/topic", "refs/tags/v1", "refs/tags/v2"]);
    assert_eq!(refs.get("refs/heads/topic").unwrap().to_string(), git(&copy, &["rev-parse", "topic"]));
    // The annotated tag is listed as its tag object, as a node does.
    assert_eq!(git(&copy, &["cat-file", "-t", &refs.get("refs/tags/v2").unwrap().to_string()]), "tag");
    assert_eq!(refs.head.as_deref(), Some("refs/heads/main"));
    assert!(local::refs(&copy.join("nowhere")).is_err());
}

#[test]
fn the_log_pages_and_says_whether_there_is_more() {
    let (_tmp, copy) = copy();
    let (first, more) = local::log_page(&copy, "refs/heads/topic", 0, 2).unwrap();
    assert_eq!((first.len(), more), (2, true));
    let (rest, more) = local::log_page(&copy, "refs/heads/topic", 2, 2).unwrap();
    assert_eq!(rest.iter().map(|c| c.subject.as_str()).collect::<Vec<_>>(), ["commit 0"]);
    assert!(!more);
    // From a commit's hash too.
    let hash = git(&copy, &["rev-parse", "main"]);
    assert_eq!(local::log_page(&copy, &hash, 0, 5).unwrap().0[0].subject, "second commit");
    assert!(matches!(local::log_page(&copy, &"0".repeat(40), 0, 5), Err(Error::Git(_))));
}

#[test]
fn a_commit_says_what_it_changed() {
    let (_tmp, copy) = copy();
    let main = git(&copy, &["rev-parse", "main"]);
    let detail = local::commit(&copy, &main).unwrap();
    assert_eq!(detail.info.subject, "second commit");
    assert_eq!(detail.message.trim_end(), "second commit\n\nwith a body");
    assert_eq!(detail.parents.len(), 1);
    assert!(!detail.email.is_empty() && !detail.more_changes);
    let changed: Vec<(&str, local::ChangeKind)> = detail.changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    for path in ["README.md", "blob.bin", "long.txt", "src/deep/x.txt", "src/main.rs"] {
        assert!(changed.contains(&(path, local::ChangeKind::Added)), "{changed:?}");
    }
    let topic = local::commit(&copy, "refs/heads/topic").unwrap();
    assert_eq!(topic.changes, [local::Change { path: "topic.txt".into(), kind: local::ChangeKind::Added }]);
    // The first commit adds all it has.
    let first = git(&copy, &["rev-list", "--max-parents=0", "main"]);
    let root = local::commit(&copy, &first).unwrap();
    assert!(root.parents.is_empty() && !root.changes.is_empty());
}

#[test]
fn changes_and_removals_are_told_apart() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, copy) = (tmp.path().join("work"), tmp.path().join("copy.git"));
    work_repo(&work, 1);
    std::fs::create_dir_all(work.join("dir")).unwrap();
    std::fs::write(work.join("dir/a.txt"), "a").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "add"]);
    std::fs::write(work.join("file0.txt"), "changed").unwrap();
    std::fs::remove_dir_all(work.join("dir")).unwrap();
    // A file where a folder was.
    std::fs::write(work.join("dir"), "now a file").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", "change"]);
    bare_repo(&copy);
    git(&work, &["push", "-q", copy.to_str().unwrap(), "main"]);
    let detail = local::commit(&copy, "refs/heads/main").unwrap();
    let changed: Vec<(&str, local::ChangeKind)> = detail.changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
    use local::ChangeKind::*;
    assert_eq!(changed, [("dir/a.txt", Deleted), ("dir", Added), ("file0.txt", Modified)]);
}

#[test]
fn what_a_path_is_and_whole_files_up_to_a_size() {
    let (_tmp, copy) = copy();
    assert_eq!(local::kind(&copy, "refs/heads/main", "").unwrap(), None);
    assert_eq!(local::kind(&copy, "refs/heads/main", "src").unwrap(), Some(EntryKind::Dir));
    assert_eq!(local::kind(&copy, "refs/heads/main", "src/main.rs").unwrap(), Some(EntryKind::File));
    assert!(local::kind(&copy, "refs/heads/main", "../x").is_err());
    assert_eq!(local::read(&copy, "refs/heads/main", "blob.bin", 5).unwrap(), [0u8, 1, 2, 255, 254]);
    assert!(matches!(local::read(&copy, "refs/heads/main", "blob.bin", 4), Err(Error::TooLarge { size: 5, .. })));
    assert!(matches!(local::read(&copy, "refs/heads/main", "src", 100), Err(Error::InvalidInput(_))));
    assert!(matches!(local::read(&copy, "refs/heads/main", "", 100), Err(Error::InvalidInput(_))));
    let hash = git(&copy, &["rev-parse", "topic"]);
    assert_eq!(local::read(&copy, &hash, "topic.txt", 10).unwrap(), b"t");
}
