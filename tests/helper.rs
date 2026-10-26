mod common;

use common::*;
use rngit::{Client, RepoUrl};

fn url() -> RepoUrl {
    "rns://727ca184e3d7e83e0e35146940920442/public/demo".parse().unwrap()
}

/// Run a scripted helper conversation and return what the helper wrote.
async fn talk(client: &Client<MockNode>, git_dir: &std::path::Path, script: &str) -> (String, Result<(), rngit::Error>) {
    let mut output = Vec::new();
    let result = rngit::helper::run(client, &url(), Some(git_dir), &mut script.as_bytes(), &mut output).await;
    (String::from_utf8(output).unwrap(), result)
}

#[tokio::test]
async fn capabilities_and_options() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    let (text, result) = talk(&client, tmp.path(), "capabilities\n\noption verbosity 2\n\n").await;
    result.unwrap();
    assert_eq!(text, "fetch\npush\noption\n\nunsupported\n");
}

#[tokio::test]
async fn list_then_fetch_adds_objects_but_no_refs() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server, local) = (tmp.path().join("work"), tmp.path().join("server.git"), tmp.path().join("local.git"));
    work_repo(&work, 3);
    bare_repo(&server);
    bare_repo(&local);
    git(&work, &["push", "-q", server.to_str().unwrap(), "main"]);
    let tip = git(&work, &["rev-parse", "main"]);
    let client = Client::new(MockNode::new(server));

    let script = format!("list\nfetch {tip} HEAD\nfetch {tip} refs/heads/main\n\n");
    let (text, result) = talk(&client, &local, &script).await;
    result.unwrap();
    assert_eq!(text, format!("{tip} refs/heads/main\n@refs/heads/main HEAD\n\n\n"));
    git(&local, &["cat-file", "-e", &tip]);
    assert_eq!(git(&local, &["for-each-ref"]), "", "git sets refs itself; the helper must not");
    git(&local, &["fsck", "--strict"]);
}

#[tokio::test]
async fn push_reports_ok_and_refusals_per_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, other, server) = (tmp.path().join("work"), tmp.path().join("other"), tmp.path().join("server.git"));
    work_repo(&work, 2);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server.clone()));

    let (text, result) = talk(&client, &work.join(".git"), "list for-push\n\npush refs/heads/main:refs/heads/main\n\n").await;
    result.unwrap();
    assert!(text.ends_with("ok refs/heads/main\n\n"), "{text}");
    assert_eq!(git(&server, &["rev-parse", "main"]), git(&work, &["rev-parse", "main"]));

    // A diverged history is refused without force and accepted with it; deleting is refused.
    work_repo(&other, 3);
    let script = "push refs/heads/main:refs/heads/main\npush :refs/heads/gone\npush :nonsense\n\n";
    let (text, result) = talk(&client, &other.join(".git"), script).await;
    result.unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("error refs/heads/main "), "{text}");
    assert_eq!(lines[1], "ok refs/heads/gone");
    assert!(lines[2].starts_with("error nonsense "), "{text}");
    assert_eq!(lines[3], "");

    // Deleting a branch that exists removes it.
    git(&work, &["push", "-q", server.to_str().unwrap(), "main:refs/heads/extra"]);
    let (text, result) = talk(&client, &work.join(".git"), "push :refs/heads/extra\n\n").await;
    result.unwrap();
    assert_eq!(text, "ok refs/heads/extra\n\n");
    assert!(gix::open(&server).unwrap().find_reference("refs/heads/extra").is_err());

    let (text, result) = talk(&client, &other.join(".git"), "push +refs/heads/main:refs/heads/main\n\n").await;
    result.unwrap();
    assert_eq!(text, "ok refs/heads/main\n\n");
    assert_eq!(git(&server, &["rev-parse", "main"]), git(&other, &["rev-parse", "main"]));
}

#[tokio::test]
async fn bad_input_is_an_error_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tmp.path().join("server.git");
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));
    for script in [
        "frobnicate\n",
        "fetch nothex refs/heads/main\n\n",
        "fetch\n\n",
        "push refs/heads/main\n\n",
        "push refs/heads/main:refs/heads/main\nfetch 00 x\n\n",
    ] {
        let (_, result) = talk(&client, tmp.path(), script).await;
        assert!(result.is_err(), "{script:?}");
    }
    let long = format!("{}\n", "x".repeat(70_000));
    let (_, result) = talk(&client, tmp.path(), &long).await;
    assert!(result.is_err());
    // Blank lines and end of input are fine.
    let (text, result) = talk(&client, tmp.path(), "\n\n").await;
    result.unwrap();
    assert_eq!(text, "");
}

#[tokio::test]
async fn list_works_without_a_git_dir_and_a_push_lists_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (work, server) = (tmp.path().join("work"), tmp.path().join("server.git"));
    work_repo(&work, 1);
    git(&work, &["branch", "b"]);
    bare_repo(&server);
    let client = Client::new(MockNode::new(server));

    // `git ls-remote` runs the helper outside any repository.
    let mut output = Vec::new();
    rngit::helper::run(&client, &url(), None, &mut "list\n".as_bytes(), &mut output).await.unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "@refs/heads/main HEAD\n\n");
    let mut output = Vec::new();
    let err = rngit::helper::run(
        &client,
        &url(),
        None,
        &mut "fetch 0000000000000000000000000000000000000001 refs/heads/x\n\n".as_bytes(),
        &mut output,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("GIT_DIR"), "{err}");

    // Two refs in one push: one listing, as stock rngit does.
    let before = client.transport().requests().len();
    let script = "list for-push\n\npush refs/heads/main:refs/heads/main\npush refs/heads/b:refs/heads/b\n\n";
    let (text, result) = talk(&client, &work.join(".git"), script).await;
    result.unwrap();
    assert!(text.ends_with("ok refs/heads/main\nok refs/heads/b\n\n"), "{text}");
    let lists = client.transport().requests().split_off(before).into_iter().filter(|(p, _)| p == "/git/list").count();
    assert_eq!(lists, 1);
}
