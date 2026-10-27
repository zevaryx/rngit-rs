//! Putting a [`Node`] on Reticulum as a `git.repositories` destination.

use std::sync::Arc;

use rns_identity::identity::Identity;
use rns_runtime::prelude::*;

use crate::client::{Client, Limits};
use crate::error::Error;
use crate::rns::RnsTransport;
use crate::server::{Node, Sender, Upstream};
use crate::transport::Response;
use crate::url::RepoUrl;

/// The destination name rngit nodes announce under.
pub const ASPECT: &str = "git.repositories";

/// The paths of the protocol the node answers.
const PATHS: [&str; 11] = [
    "/git/list",
    "/git/fetch",
    "/git/push",
    "/git/create",
    "/git/fork",
    "/git/sync",
    "/git/mirror",
    "/git/delete",
    "/mgmt/perms",
    "/mgmt/release",
    "/mgmt/work",
];

/// One request that was answered: its path, who sent it (an unidentified
/// sender has no hash) and the status byte of the answer, which is `0` for a
/// file.
pub type Activity = Arc<dyn Fn(&str, Option<[u8; 16]>, u8) + Send + Sync>;

/// Answer rngit requests from `node` at the `git.repositories` destination
/// of `identity`. The destination is returned so the caller can announce it
/// and close it; it serves until then.
pub async fn serve(
    runtime: &ReticulumHandle,
    identity: Identity,
    node: Arc<Node>,
    app_data: Option<Vec<u8>>,
    activity: Option<Activity>,
) -> Result<RegisteredDestination, Error> {
    let options = DestinationRuntimeOptions { default_app_data: app_data, ..DestinationRuntimeOptions::default() };
    let registered = runtime
        .register_destination(identity, ASPECT, options)
        .await
        .map_err(|e| Error::Transport(format!("could not register the node: {e}")))?;
    for path in PATHS {
        let (node, activity) = (node.clone(), activity.clone());
        registered
            .handle
            .register_request_handler(path, AllowPolicy::AllowAll, Vec::new(), false, move |request| {
                let peer = request.remote_identity.as_ref().map(|identity| identity.hash);
                let sender = request
                    .remote_identity
                    .as_ref()
                    .map(|identity| Sender { hash: identity.hash, key: <[u8; 64]>::try_from(identity.get_public_key().as_slice()).ok() });
                let answered = || node.handle_from(&request.path, &request.data, sender);
                // Building a bundle takes a while: let the runtime's other threads carry on meanwhile.
                let response = match tokio::runtime::Handle::try_current() {
                    Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(answered),
                    _ => answered(),
                };
                let Some(response) = response else { return RequestOutcome::Drop };
                if let Some(activity) = &activity {
                    activity(
                        &request.path,
                        peer,
                        match &response {
                            Response::Packet(p) => p.first().copied().unwrap_or(0xff),
                            Response::File { .. } => 0,
                        },
                    );
                }
                match response {
                    Response::Packet(reply) => RequestOutcome::Reply(reply),
                    Response::File { data, metadata } => RequestOutcome::ReplyFile { data, metadata, auto_compress: false },
                }
            })
            .await
            .map_err(|e| Error::Transport(format!("could not serve {path}: {e}")))?;
    }
    Ok(registered)
}

/// Fetches from other nodes over `runtime`, as `identity`, for a [`Node`]'s
/// forks, mirrors and syncs. Put it in [`Node::upstream`] before the node is
/// served. It needs the multi-threaded runtime the node's requests are
/// answered on, since a request waits for the fetch.
pub fn upstream(runtime: ReticulumHandle, identity: Identity, limits: Limits) -> Arc<dyn Upstream> {
    Arc::new(Reticulum { runtime, identity, limits })
}

struct Reticulum {
    runtime: ReticulumHandle,
    identity: Identity,
    limits: Limits,
}

impl std::fmt::Debug for Reticulum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Reticulum")
    }
}

impl Upstream for Reticulum {
    fn pull(&self, source: &RepoUrl, dest: &std::path::Path, timeout: std::time::Duration) -> Result<(), String> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| "No runtime to fetch on")?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return Err("This node can't fetch from other nodes while it runs on one thread".into());
        }
        let transport = RnsTransport::new(self.runtime.clone(), self.identity.clone(), source.destination).with_request_timeout(timeout);
        let client = Client::new(transport).with_limits(self.limits.clone());
        let fetched = tokio::task::block_in_place(|| handle.block_on(tokio::time::timeout(timeout, client.clone_into(source, dest))));
        match fetched {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("The source took too long".into()),
        }
    }
}

/// Follow the sources of the node's mirrors: every `check_every`, those that
/// haven't been brought up to date for `older_than` are. Stop it by aborting
/// the task.
pub fn keep_mirrors(node: Arc<Node>, check_every: std::time::Duration, older_than: std::time::Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(check_every).await;
            let node = node.clone();
            let _ = tokio::task::spawn_blocking(move || node.sync_mirrors(older_than)).await;
        }
    })
}
