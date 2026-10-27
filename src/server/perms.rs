//! Reading and writing who may do what: `/mgmt/perms`.
//!
//! A repository's rules are `<repo>.allowed` in its group's folder and a
//! group's are `<group>.allowed` beside the group's folder, where stock
//! rngit keeps them. A node reads `group/group.allowed` too, but changes
//! only the file stock names.

use std::io::Read as _;

use rmpv::Value;

use super::support::{check_rules, packed, refuse, text, write_file};
use super::{MAX_RULES_BYTES, Node, Perm, Refusal, ack, int_key, split_repo_path};
use crate::error::Status;
use crate::transport::Response;
use crate::wire::MapRef;

/// A group's name as a folder's, if it is one.
fn group_name(request: &Value) -> Option<String> {
    let name = int_key(request, 2)?;
    // The same rules as a repository's group.
    let probe = format!("{name}/x.git");
    split_repo_path(&probe).is_some_and(|(group, _)| group == name).then_some(name)
}

fn read_text(path: &std::path::Path) -> String {
    let mut out = String::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(MAX_RULES_BYTES).read_to_string(&mut out);
    }
    out
}

impl Node {
    /// `/mgmt/perms`: `rperms` and `gperms`, each with a `get` and a `set`
    /// step. Reading or changing a repository's rules needs its administrator,
    /// a group's the group's.
    pub(super) fn perms(&self, request: &Value, map: &MapRef, peer: &[u8; 16]) -> Result<Response, Refusal> {
        if *peer == super::NULL_IDENT {
            return Err(refuse(Status::NotAllowed, "Not identified"));
        }
        let step = map.str("step");
        match map.str("operation").as_deref() {
            Some("rperms") => {
                let name = int_key(request, 0).ok_or(refuse(Status::BadRequest, "No repository specified"))?;
                let (group, repo) = split_repo_path(&name).ok_or(refuse(Status::NotFound, "Not found"))?;
                if !self.root.join(group).join(repo).is_dir() || !self.allowed(group, repo, peer, Perm::Read) {
                    return Err(refuse(Status::NotFound, "Not found"));
                }
                if !self.allowed(group, repo, peer, Perm::Admin) {
                    return Err(refuse(Status::NotAllowed, "Not allowed"));
                }
                self.perms_step(step.as_deref(), &self.root.join(group).join(format!("{repo}.allowed")), map)
            }
            Some("gperms") => {
                let group = group_name(request).ok_or(refuse(Status::BadRequest, "No group specified"))?;
                if !self.root.join(&group).is_dir() || !self.allowed_in_group(&group, peer, Perm::Read) {
                    return Err(refuse(Status::NotFound, "Not found"));
                }
                if !self.allowed_in_group(&group, peer, Perm::Admin) {
                    return Err(refuse(Status::NotAllowed, "Not allowed"));
                }
                self.perms_step(step.as_deref(), &self.root.join(format!("{group}.allowed")), map)
            }
            _ => Err(refuse(Status::BadRequest, "Invalid request")),
        }
    }

    /// Read the rules in `file`, or replace them with what the request says.
    pub(super) fn perms_step(&self, step: Option<&str>, file: &std::path::Path, map: &MapRef) -> Result<Response, Refusal> {
        match step {
            Some("get") => packed(&Value::Map(vec![(text("content"), text(&read_text(file)))])),
            Some("set") => {
                let content = map.str("content").unwrap_or_default();
                if content.len() as u64 > MAX_RULES_BYTES {
                    return Err(refuse(Status::BadRequest, "Permissions are too long"));
                }
                check_rules(&content)?;
                write_file(file, content.as_bytes()).map_err(|_| refuse(Status::ServerError, "Error setting permissions"))?;
                Ok(ack())
            }
            Some(_) => Err(refuse(Status::BadRequest, "Invalid step")),
            None => Err(refuse(Status::BadRequest, "Invalid request")),
        }
    }
}
