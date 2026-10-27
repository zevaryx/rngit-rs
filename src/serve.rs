//! Putting a [`Node`] on Reticulum as a `git.repositories` destination.

use std::sync::Arc;

use rns_identity::identity::Identity;
use rns_runtime::prelude::*;

use crate::error::Error;
use crate::server::Node;
use crate::transport::Response;

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
                let answered = || node.handle(&request.path, &request.data, peer);
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
