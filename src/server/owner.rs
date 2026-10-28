//! What the node's owner does with it directly, with no request: list what it
//! hosts, make, fork, mirror, sync, rename and delete repositories, and read
//! and change who may use them. The owner holds the folder anyway, so the
//! rules don't apply; names are checked as a request's are.

use std::path::{Path, PathBuf};

use super::support::{check_rules, write_file};
use super::upstream::{checked_source, read_following};
use super::{Following, Follows, MAX_RULES_BYTES, NULL_IDENT, Node, Perm, dir_size, split_repo_path};

/// Most repositories listed, so a huge folder can't take long.
const MAX_LISTED: usize = 10_000;

/// A repository the node hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hosted {
    pub group: String,
    /// The folder's name, `demo.git`.
    pub name: String,
    /// What it follows, for a fork or a mirror.
    pub following: Option<Following>,
    /// Bytes it takes on disk, with its releases.
    pub bytes: u64,
    /// Whether anyone may read (clone) it. If not, only those its rules or its
    /// group's name may, and to anyone else it isn't there.
    pub open: bool,
}

/// Whether a group's name is one a request could name.
fn plain_group(group: &str) -> bool {
    split_repo_path(&format!("{group}/x.git")).is_some()
}

fn plain_repo(group: &str, repo: &str) -> bool {
    split_repo_path(&format!("{group}/{repo}")).is_some()
}

fn checked(group: &str, repo: &str) -> Result<(), String> {
    plain_repo(group, repo)
        .then_some(())
        .ok_or_else(|| format!("\"{group}/{repo}\" isn't a group and a name.git made of letters, digits and -_."))
}

/// What sits beside a repository and goes where it goes: its rules, releases and work documents.
fn beside(group_dir: &Path, repo: &str) -> [PathBuf; 3] {
    ["allowed", "releases", "work"].map(|what| group_dir.join(format!("{repo}.{what}")))
}

fn read_text(path: &Path) -> String {
    use std::io::Read as _;
    let mut text = String::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(MAX_RULES_BYTES).read_to_string(&mut text);
    }
    text
}

impl Node {
    /// The groups, by name.
    pub fn groups(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else { return Vec::new() };
        let mut groups: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|g| plain_group(g))
            .collect();
        groups.sort();
        groups
    }

    /// Every repository the node hosts, by group and name.
    pub fn hosted(&self) -> Vec<Hosted> {
        let mut out = Vec::new();
        for group in self.groups() {
            let Ok(entries) = std::fs::read_dir(self.root.join(&group)) else { continue };
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| plain_repo(&group, name))
                .collect();
            names.sort();
            for name in names {
                if out.len() >= MAX_LISTED {
                    return out;
                }
                let dir = self.root.join(&group).join(&name);
                let releases = self.root.join(&group).join(format!("{name}.releases"));
                let bytes = dir_size(&dir).saturating_add(dir_size(&releases));
                let open = self.allowed(&group, &name, &NULL_IDENT, Perm::Read);
                out.push(Hosted { group: group.clone(), name, following: read_following(&dir), bytes, open });
            }
        }
        out
    }

    /// Make a group, if there isn't one.
    pub fn make_group(&self, group: &str) -> Result<(), String> {
        if !plain_group(group) {
            return Err(format!("\"{group}\" isn't a group name made of letters, digits and -_."));
        }
        std::fs::create_dir_all(self.root.join(group)).map_err(|e| format!("Couldn't make the group {group}: {e}"))
    }

    fn room_for(&self, group: &str, repo: &str) -> Result<PathBuf, String> {
        self.make_group(group)?;
        let group_dir = self.root.join(group);
        if group_dir.join(repo).exists() {
            return Err(format!("{group}/{repo} is already there"));
        }
        Ok(group_dir)
    }

    /// Make an empty repository, `repo` being `name.git`, and its group if
    /// there isn't one. `admin` becomes its administrator, as whoever makes
    /// one by a request does.
    pub fn make_repository(&self, group: &str, repo: &str, admin: Option<&[u8; 16]>) -> Result<(), String> {
        checked(group, repo)?;
        let group_dir = self.room_for(group, repo)?;
        let dir = group_dir.join(repo);
        let failed = |e: String| format!("Couldn't make {group}/{repo}: {e}");
        gix::init_bare(&dir).map_err(|e| failed(e.to_string()))?;
        std::fs::write(dir.join("HEAD"), "ref: refs/heads/master\n").map_err(|e| failed(e.to_string()))?;
        if let Some(admin) = admin {
            write_file(&group_dir.join(format!("{repo}.allowed")), format!("adm:{}", hex::encode(admin)).as_bytes())
                .map_err(|e| failed(e.to_string()))?;
        }
        Ok(())
    }

    /// Fork or mirror `source` (`rns://` or `https://`) as `group/repo`,
    /// making the group if there isn't one. It fetches, so it takes as long
    /// as that does: call it off any async thread.
    pub fn follow_source(&self, group: &str, repo: &str, source: &str, kind: Follows, admin: Option<&[u8; 16]>) -> Result<(), String> {
        checked(group, repo)?;
        let parsed = checked_source(source.trim(), self.open_sources).map_err(|r| r.1)?;
        let group_dir = self.room_for(group, repo)?;
        self.make_following(&group_dir, repo, source.trim(), &parsed, kind, admin).map_err(|r| r.1)
    }

    /// Bring a fork or mirror up to date with its source. It fetches, as
    /// [`Node::follow_source`] does.
    pub fn sync_repository(&self, group: &str, repo: &str) -> Result<(), String> {
        checked(group, repo)?;
        let dir = self.root.join(group).join(repo);
        if !dir.is_dir() {
            return Err(format!("{group}/{repo} isn't there"));
        }
        let following = read_following(&dir).ok_or_else(|| format!("{group}/{repo} is neither a fork nor a mirror"))?;
        self.follow(&dir, &following)
    }

    /// Move a repository, with its rules, releases and work documents, to
    /// another name or group (made if there isn't one).
    pub fn rename_repository(&self, group: &str, repo: &str, to_group: &str, to_repo: &str) -> Result<(), String> {
        checked(group, repo)?;
        checked(to_group, to_repo)?;
        let from_dir = self.root.join(group);
        if !from_dir.join(repo).is_dir() {
            return Err(format!("{group}/{repo} isn't there"));
        }
        let to_dir = self.room_for(to_group, to_repo)?;
        if beside(&to_dir, to_repo).iter().any(|p| p.exists()) {
            return Err(format!("{to_group}/{to_repo} still has rules, releases or work documents of an older repository"));
        }
        std::fs::rename(from_dir.join(repo), to_dir.join(to_repo)).map_err(|e| format!("Couldn't move {group}/{repo}: {e}"))?;
        for (from, to) in beside(&from_dir, repo).into_iter().zip(beside(&to_dir, to_repo)) {
            if from.exists() {
                std::fs::rename(&from, &to).map_err(|e| format!("Moved {group}/{repo}, but not {}: {e}", from.display()))?;
            }
        }
        Ok(())
    }

    /// Delete a repository, with its rules, releases and work documents.
    pub fn delete_repository(&self, group: &str, repo: &str) -> Result<(), String> {
        checked(group, repo)?;
        let group_dir = self.root.join(group);
        let dir = group_dir.join(repo);
        if !dir.is_dir() {
            return Err(format!("{group}/{repo} isn't there"));
        }
        // Gone first, so nothing serves it half deleted.
        std::fs::remove_dir_all(&dir).map_err(|e| format!("Couldn't delete {group}/{repo}: {e}"))?;
        for path in beside(&group_dir, repo) {
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
        Ok(())
    }

    /// The rules file of a repository (`repo` given) or of a group: the one a
    /// `/mgmt/perms` request reads and writes.
    fn rules_file(&self, group: &str, repo: Option<&str>) -> Result<PathBuf, String> {
        match repo {
            Some(repo) => checked(group, repo).map(|()| self.root.join(group).join(format!("{repo}.allowed"))),
            None if plain_group(group) => Ok(self.root.join(format!("{group}.allowed"))),
            None => Err(format!("\"{group}\" isn't a group name made of letters, digits and -_.")),
        }
    }

    /// The rules written for a repository or a group, as text.
    pub fn rules(&self, group: &str, repo: Option<&str>) -> Result<String, String> {
        self.rules_file(group, repo).map(|path| read_text(&path))
    }

    /// Replace the rules of a repository or a group. Each line is a rule, as a
    /// `/mgmt/perms` request has to give them; empty text removes them.
    pub fn set_rules(&self, group: &str, repo: Option<&str>, text: &str) -> Result<(), String> {
        let path = self.rules_file(group, repo)?;
        if repo.is_some_and(|repo| !self.root.join(group).join(repo).is_dir()) || !self.root.join(group).is_dir() {
            return Err("There is no such repository or group".into());
        }
        if text.len() as u64 > MAX_RULES_BYTES {
            return Err("The rules are too long".into());
        }
        check_rules(text).map_err(|r| r.1)?;
        if text.trim().is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("Couldn't remove {}: {e}", path.display())),
                _ => Ok(()),
            };
        }
        write_file(&path, text.as_bytes()).map_err(|e| format!("Couldn't write {}: {e}", path.display()))
    }
}
