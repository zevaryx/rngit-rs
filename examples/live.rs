//! Drives the client against a live rngit node.
//!
//! `cargo run --example live -- <reticulum config dir> rns://<hash>/<group>/<repo>`
//! The group must allow everything for the anonymous test identity. `BIG_MB` sets the size of the large push. `SOURCE` is an rns:// URL on a second node, used for fork and mirror.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rngit::rns::RnsTransport;
use rngit::{Client, RepoUrl};
use rns_identity::identity::Identity;
use rns_runtime::prelude::*;

fn step<T: std::fmt::Debug, E: std::fmt::Debug>(name: &str, result: Result<T, E>) {
    match result {
        Ok(v) => println!("OK   {name}: {}", format!("{v:?}").chars().take(200).collect::<String>()),
        Err(e) => println!("FAIL {name}: {e:?}"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let config = args.next().ok_or("usage: live <config dir> <rns:// url>")?;
    let repo: RepoUrl = args.next().ok_or("missing url")?.parse()?;
    let shutdown = ShutdownSignal::new();
    let runtime = init(Some(&config), None, shutdown, Arc::new(AtomicBool::new(true))).await?;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let client = Client::new(RnsTransport::new(runtime.clone(), Identity::new(), repo.destination));
    let refs = client.list(&repo, false).await?;
    println!("refs: {refs:?}");
    let dest = std::env::temp_dir().join("rngit-live-clone.git");
    let _ = std::fs::remove_dir_all(&dest);
    client.clone_into(&repo, &dest).await?;
    println!("cloned into {}", dest.display());

    // Repository management and push, small then large.
    let identity = Identity::new();
    let client =
        Client::new(RnsTransport::new(runtime.clone(), identity.clone(), repo.destination).with_request_timeout(Duration::from_secs(300)));
    let name = format!("t{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs());
    let target: RepoUrl = format!("rns://{}/public/{}.git", hex::encode(repo.destination), name).parse()?;
    step("create", client.create(&target).await);
    let work = std::env::temp_dir().join("rngit-live-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(&work)
            .args(["-c", "commit.gpgsign=false", "-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(work.join("a.txt"), "hello\n")?;
    git(&["add", "."]);
    git(&["commit", "-qm", "one"]);
    step("push small", client.push_from(&target, &work.join(".git"), "refs/heads/main", "refs/heads/main", false).await);
    let mut big = vec![0u8; std::env::var("BIG_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(3usize) * 1024 * 1024];
    let mut x = 0x9e3779b9u32;
    for b in big.iter_mut() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        *b = (x >> 24) as u8;
    }
    std::fs::write(work.join("big.bin"), &big)?;
    git(&["add", "."]);
    git(&["commit", "-qm", "big"]);
    step("push big", client.push_from(&target, &work.join(".git"), "refs/heads/main", "refs/heads/main", false).await);
    git(&["tag", "v1.0"]);
    step("push tag", client.push_from(&target, &work.join(".git"), "refs/tags/v1.0", "refs/tags/v1.0", false).await);
    step("list pushed", client.list(&target, false).await.map(|r| r.refs.len()));

    // Releases and work documents.
    let signer = rngit::rns::IdentitySigner(&identity);
    let files = vec![("hello.txt".to_string(), b"release file".to_vec())];
    step(
        "create_release",
        client.create_release(&target, &signer, "v1.0", None, "notes", &files, "2026-10-26T00:00:00Z", 1_793_000_000).await,
    );
    step("releases", client.releases(&target).await.map(|r| format!("{r:?}")));
    step("verified release", client.fetch_verified_release(&target, "v1.0").await.map(|(m, f)| (format!("{m:?}").len(), f.len())));
    step("work_create", client.work_create(&target, &signer, rngit::client::WorkKind::Create, "title", "body", "markdown").await);
    step("work_list", client.work_list(&target, rngit::client::WorkScope::All).await.map(|l| l.active.len()));
    step("repo_perms", client.repo_perms(&target).await);

    // Everything else the node offers.
    let hash = hex::encode(repo.destination);
    // The source must be another node: a node cannot fetch from itself over Reticulum.
    let src = std::env::var("SOURCE").unwrap_or_else(|_| format!("rns://{hash}/public/{name}.git"));
    let fork: RepoUrl = format!("rns://{hash}/public/{name}f.git").parse()?;
    let mirror: RepoUrl = format!("rns://{hash}/public/{name}m.git").parse()?;
    step("fork", client.fork(&fork, &src).await);
    step("mirror", client.mirror(&mirror, &src).await);
    step("sync fork", client.sync(&fork).await);
    step("sync mirror", client.sync(&mirror).await);
    step("list fork", client.list(&fork, false).await.map(|r| r.refs.len()));
    step("set_repo_perms", client.set_repo_perms(&target, &format!("adm:{}\nr:all\n", hex::encode(identity.hash))).await);
    step("repo_perms again", client.repo_perms(&target).await);
    step("group_perms", client.group_perms("public").await);
    step("set_group_perms", client.set_group_perms("public", "r:all\nw:all\nrel:all\nc:all\ni:all\np:all\n").await);
    use rngit::client::WorkScope;
    step("work_edit", client.work_edit(&target, &signer, 1, WorkScope::Active, "title2", "body2").await);
    step("work_comment", client.work_comment(&target, 1, WorkScope::Active, "a comment", "markdown").await);
    step("work_view", client.work_view(&target, 1, WorkScope::Active).await.map(|d| format!("{d:?}")));
    step("work_perms", client.work_perms(&target, 1).await);
    step("set_work_perms", client.set_work_perms(&target, 1, &format!("adm:{}\n", hex::encode(identity.hash))).await);
    step("work_complete", client.work_complete(&target, 1).await);
    step("work_activate", client.work_activate(&target, 1).await);
    step("work_propose", client.work_create(&target, &signer, rngit::client::WorkKind::Propose, "prop", "p", "markdown").await);
    step("work_delete", client.work_delete(&target, 1, WorkScope::Active).await);
    step("fetch_artifact", client.fetch_artifact(&target, "v1.0", "hello.txt").await.map(|d| d.len()));
    step("delete_release", client.delete_release(&target, "v1.0").await);
    step("releases after", client.releases(&target).await.map(|r| r.releases.len()));
    runtime.shutdown_and_wait().await;
    Ok(())
}
