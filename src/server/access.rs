//! Who may do what, resolved as stock rngit resolves it.
//!
//! Each level (a work document, a repository, a group) has lists of who
//! holds each permission. A level that names anyone for a permission decides
//! it for the levels above, with these quirks of stock's that are kept so
//! the same files mean the same on either node:
//!
//! - `none` in a list beats `all`, and both beat naming someone;
//! - an administrator of a repository holds everything there, but one of the
//!   group only where the repository doesn't list the permission itself;
//! - a repository that lists a permission for anyone leaves the group's list
//!   out, so `r:<someone>` on a repository makes it theirs alone, even if the
//!   group says `r:all`.

use super::{Perm, Rule, Who};

const SLOTS: usize = 8;

fn slot(perm: Perm) -> usize {
    match perm {
        Perm::Read => 0,
        Perm::Write => 1,
        Perm::Create => 2,
        Perm::Stats => 3,
        Perm::Release => 4,
        Perm::Issues => 5,
        Perm::Proposals => 6,
        Perm::Admin => 7,
    }
}

/// Who holds each permission at one level.
#[derive(Debug, Clone, Default)]
pub(super) struct Grants {
    lists: [Vec<Who>; SLOTS],
}

impl Grants {
    pub(super) fn from_rules<'a>(rules: impl IntoIterator<Item = &'a Rule>) -> Self {
        let mut grants = Grants::default();
        for rule in rules {
            for perm in &rule.perms {
                let list = &mut grants.lists[slot(*perm)];
                if !list.contains(&rule.who) {
                    list.push(rule.who);
                }
            }
        }
        grants
    }

    fn list(&self, perm: Perm) -> &[Who] {
        &self.lists[slot(perm)]
    }
}

/// What a list says of `me` by itself: no for `none`, yes for `all` or for
/// them, and nothing if it names neither.
fn says(list: &[Who], me: &[u8; 16]) -> Option<bool> {
    if list.contains(&Who::Nobody) {
        Some(false)
    } else if list.contains(&Who::Everyone) || list.contains(&Who::Identity(*me)) {
        Some(true)
    } else {
        None
    }
}

fn administers(grants: &Grants, me: &[u8; 16]) -> bool {
    grants.list(Perm::Admin).contains(&Who::Identity(*me))
}

/// Whether `me` holds `perm` for a repository.
pub(super) fn repository(me: &[u8; 16], perm: Perm, repo: &Grants, group: &Grants) -> bool {
    let own = repo.list(perm);
    if let Some(answer) = says(own, me) {
        return answer;
    }
    if administers(repo, me) {
        return true;
    }
    if !own.is_empty() {
        return false;
    }
    says(group.list(perm), me).unwrap_or_else(|| administers(group, me))
}

/// Whether `me` holds `perm` for a group (making repositories in it, say).
pub(super) fn in_group(me: &[u8; 16], perm: Perm, group: &Grants) -> bool {
    says(group.list(perm), me).unwrap_or_else(|| administers(group, me))
}

/// Whether `me` holds `perm` for a work document: its own list first, then
/// the repository's and the group's.
pub(super) fn document(me: &[u8; 16], perm: Perm, doc: &Grants, repo: &Grants, group: &Grants) -> bool {
    if let Some(answer) = says(doc.list(perm), me) {
        return answer;
    }
    if administers(doc, me) {
        return true;
    }
    repository(me, perm, repo, group)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: [u8; 16] = [7; 16];
    const OTHER: [u8; 16] = [8; 16];

    fn grants(text: &str) -> Grants {
        Grants::from_rules(&Rule::parse(&text.replace("ME", &hex::encode(ME)).replace("OTHER", &hex::encode(OTHER))))
    }

    #[test]
    fn a_repository_that_lists_a_permission_leaves_the_groups_out() {
        let group = grants("r:all");
        assert!(repository(&ME, Perm::Read, &grants(""), &group));
        // Naming someone makes it theirs alone.
        let repo = grants("r:OTHER");
        assert!(!repository(&ME, Perm::Read, &repo, &group));
        assert!(repository(&OTHER, Perm::Read, &repo, &group));
        // And for what it doesn't list, the group still decides.
        assert!(repository(&ME, Perm::Read, &grants("w:all"), &group));
    }

    #[test]
    fn none_beats_all_and_all_beats_a_name() {
        assert!(!repository(&ME, Perm::Read, &grants("r:all\nr:none"), &grants("r:all")));
        assert!(!repository(&ME, Perm::Read, &grants(""), &grants("r:all, r:none")));
        assert!(repository(&ME, Perm::Read, &grants("r:all"), &grants("r:none")));
        // A repository's administrator reads, even where `none` is said of the group.
        let admin = grants("adm:ME");
        assert!(repository(&ME, Perm::Read, &admin, &grants("r:none")));
    }

    #[test]
    fn a_groups_administrator_holds_what_the_repository_leaves_unsaid() {
        let group = grants("adm:ME");
        assert!(repository(&ME, Perm::Write, &grants(""), &group));
        assert!(!repository(&ME, Perm::Write, &grants("w:none"), &group));
        // The repository's own administrator holds it even where it lists others.
        let repo = grants("w:OTHER\nadm:ME");
        assert!(repository(&ME, Perm::Write, &repo, &grants("")));
        // The group's doesn't, where the repository lists the permission.
        let repo = grants("w:OTHER");
        assert!(!repository(&ME, Perm::Write, &repo, &group));
        assert!(in_group(&ME, Perm::Create, &group));
        assert!(!in_group(&OTHER, Perm::Create, &group));
    }

    #[test]
    fn a_document_is_decided_by_itself_before_its_repository() {
        let (repo, group) = (grants("i:all"), grants(""));
        assert!(document(&ME, Perm::Issues, &grants(""), &repo, &group));
        assert!(!document(&ME, Perm::Issues, &grants("i:none"), &repo, &group));
        let owner = grants("i:ME\nw:ME");
        assert!(document(&ME, Perm::Write, &owner, &grants(""), &group));
        assert!(!document(&OTHER, Perm::Write, &owner, &grants(""), &group));
    }
}
