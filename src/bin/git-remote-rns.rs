//! `git-remote-rns`: lets `git clone`, `fetch` and `push` use `rns://` URLs.
//!
//! Git runs it as `git-remote-rns <remote-name> <url>`. See
//! [`rngit::session`] for the settings it reads.

use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use rngit::rns::RnsTransport;
use rngit::session::Session;
use rngit::{Client, RepoUrl};

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("git-remote-rns: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (_remote, url) = (args.next(), args.next().ok_or("usage: git-remote-rns <remote-name> <url>")?);
    let repo: RepoUrl = url.parse()?;
    let git_dir = std::env::var_os("GIT_DIR").map(PathBuf::from);

    let session = Session::start(Session::settle_time()).await?;
    let transport = RnsTransport::new(session.runtime.clone(), session.identity.clone(), repo.destination)
        .with_request_timeout(Duration::from_secs(600));
    let client = Client::new(transport);
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let result = rngit::helper::run(&client, &repo, git_dir.as_deref(), &mut BufReader::new(stdin.lock()), &mut output).await;
    output.flush().ok();
    client.transport().disconnect().await;
    session.runtime.shutdown_and_wait().await;
    result?;
    Ok(())
}
