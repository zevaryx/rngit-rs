//! `rngit`: manage repositories, releases and work documents on rngit nodes.
//! Run `rngit help` for the commands. See [`rngit::session`] for settings.

use std::time::Duration;

use rngit::discovery::discover;
use rngit::rns::{IdentitySigner, RnsTransport};
use rngit::session::Session;
use rngit::{Client, RepoUrl};

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("rngit: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    if args.first().is_none_or(|a| matches!(a.as_str(), "help" | "-h" | "--help")) {
        print!("{}", rngit::cli::USAGE);
        return Ok(());
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let session = Session::start(Session::settle_time()).await?;
    let result = if args[0] == "nodes" {
        let seconds = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
        let nodes = discover(&session.runtime, Duration::from_secs(seconds)).await?;
        for n in &nodes {
            println!("rns://{}  {} hops  {}", hex::encode(n.destination), n.hops, n.name.as_deref().unwrap_or(""));
        }
        if nodes.is_empty() {
            println!("no nodes announced in {seconds} seconds");
        }
        Ok(())
    } else {
        let repo: RepoUrl = args.get(1).ok_or("missing URL; try `rngit help`")?.parse()?;
        let transport = RnsTransport::new(session.runtime.clone(), session.identity.clone(), repo.destination);
        let client = Client::new(transport.with_request_timeout(Duration::from_secs(600)));
        let signer = IdentitySigner(&session.identity);
        let result = rngit::cli::run(&client, &signer, now, &args, &mut std::io::stdin().lock(), &mut out).await;
        client.transport().disconnect().await;
        result
    };
    session.runtime.shutdown_and_wait().await;
    Ok(result?)
}
