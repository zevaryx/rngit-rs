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
    if rngit::cli::is_local(&args[0]) {
        return Ok(rngit::cli::local_command(&args[0], &args[1..], &mut out)?);
    }
    if args[0] == "serve" {
        return serve(&args[1..]).await;
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

/// `rngit serve DIR [RULES]`: answer rngit requests from the repositories in
/// DIR (`group/name.git`) until stopped. RULES are read for every group, as in
/// `.allowed` files; the default is `r:none`, so only `.allowed` files let anyone in.
async fn serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let dir = args.first().ok_or("missing DIR; try `rngit help`")?;
    let mut node = rngit::server::Node::new(dir.into());
    node.default_rules = rngit::server::Rule::parse(args.get(1).map_or("", String::as_str));
    let session = Session::start(Duration::from_secs(1)).await?;
    let config = std::env::var_os("RNGIT_CONFIG").map(std::path::PathBuf::from);
    let config = config.unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".rngit"));
    let identity = rngit::session::load_identity_named(&config, "node_identity")?;
    let log: rngit::serve::Activity = std::sync::Arc::new(|path, peer, status| {
        eprintln!("{path} from {} -> {status}", peer.map_or("an unidentified peer".to_string(), hex::encode));
    });
    let registered = rngit::serve::serve(&session.runtime, identity, std::sync::Arc::new(node), None, Some(log)).await?;
    println!("serving rns://{}", hex::encode(registered.handle.destination_hash()));
    // Stock waits a few seconds so the interfaces are up before the first announce.
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        let _ = registered.handle.announce(Default::default()).await;
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    }
}
