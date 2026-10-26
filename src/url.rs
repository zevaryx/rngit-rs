//! `rns://<destination hash>/<group>/<repo>` URLs.

use std::fmt;
use std::str::FromStr;

use crate::error::Error;

/// A repository on a node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoUrl {
    pub destination: [u8; 16],
    pub group: String,
    pub repo: String,
}

fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part.len() <= 128
        && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl RepoUrl {
    pub fn new(destination: [u8; 16], group: &str, repo: &str) -> Result<Self, Error> {
        let repo = repo.strip_suffix(".git").unwrap_or(repo);
        if !valid_part(group) || !valid_part(repo) {
            return Err(Error::InvalidInput(format!("bad repository name {group}/{repo}")));
        }
        Ok(RepoUrl { destination, group: group.to_string(), repo: repo.to_string() })
    }

    /// The repository path as the node expects it, `group/repo.git`.
    pub fn repo_path(&self) -> String {
        format!("{}/{}.git", self.group, self.repo)
    }
}

impl FromStr for RepoUrl {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let rest = s.strip_prefix("rns://").ok_or_else(|| Error::InvalidInput("URL must start with rns://".into()))?;
        let mut parts = rest.split('/');
        let hash = parts.next().unwrap_or_default();
        let (group, repo) = match (parts.next(), parts.next(), parts.next()) {
            (Some(group), Some(repo), None) => (group, repo),
            _ => return Err(Error::InvalidInput("URL must be rns://<hash>/<group>/<repo>".into())),
        };
        let bytes = hex::decode(hash).map_err(|_| Error::InvalidInput("destination hash is not hex".into()))?;
        let destination: [u8; 16] = bytes.try_into().map_err(|_| Error::InvalidInput("destination hash must be 16 bytes".into()))?;
        RepoUrl::new(destination, group, repo)
    }
}

impl fmt::Display for RepoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rns://{}/{}/{}", hex::encode(self.destination), self.group, self.repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "727ca184e3d7e83e0e35146940920442";

    #[test]
    fn parses_and_prints() {
        let url: RepoUrl = format!("rns://{HASH}/public/demo").parse().unwrap();
        assert_eq!(url.repo_path(), "public/demo.git");
        assert_eq!(url.to_string(), format!("rns://{HASH}/public/demo"));
    }

    #[test]
    fn accepts_git_suffix() {
        let url: RepoUrl = format!("rns://{HASH}/public/demo.git").parse().unwrap();
        assert_eq!(url.repo, "demo");
    }

    #[test]
    fn rejects_bad_urls() {
        for bad in [
            "http://x/y/z".to_string(),
            format!("rns://{HASH}/public"),
            format!("rns://{HASH}/public/demo/extra"),
            format!("rns://{HASH}/../demo"),
            format!("rns://{HASH}/public/.."),
            format!("rns://{HASH}/pub lic/demo"),
            "rns://abcd/public/demo".to_string(),
            "rns://zz7ca184e3d7e83e0e35146940920442/public/demo".to_string(),
        ] {
            assert!(bad.parse::<RepoUrl>().is_err(), "{bad}");
        }
    }
}
