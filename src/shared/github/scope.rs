//! Where a runner is registered, parsed from its `.runner` `gitHubUrl`.

use std::fmt;

/// A GitHub host: `github.com`, a GHE.com data-residency tenant, or a GHES server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitHubHost(String);

impl GitHubHost {
    pub fn dotcom() -> Self {
        Self("github.com".to_string())
    }

    pub fn parse(host: &str) -> Result<Self, String> {
        let host = host.trim().to_ascii_lowercase();
        let valid = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b));
        if valid {
            Ok(Self(host))
        } else {
            Err(format!("{host:?} is not a host name"))
        }
    }

    pub fn is_dotcom(&self) -> bool {
        self.0 == "github.com"
    }

    /// REST API root: `api.github.com`, `api.<tenant>.ghe.com`, or `<host>/api/v3` (GHES).
    pub fn api_base(&self) -> String {
        if self.is_dotcom() {
            "https://api.github.com".to_string()
        } else if self.0.ends_with(".ghe.com") {
            format!("https://api.{}", self.0)
        } else {
            format!("https://{}/api/v3", self.0)
        }
    }
}

impl fmt::Display for GitHubHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a runner is registered to. Enterprise runners can only be listed with a
/// classic token, which ghr-stats does not accept.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Owner {
    Org(String),
    Repo { owner: String, repo: String },
    Enterprise(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunnerScope {
    pub host: GitHubHost,
    pub owner: Owner,
}

impl RunnerScope {
    /// `https://<host>/<org>`, `https://<host>/<owner>/<repo>` or
    /// `https://<host>/enterprises/<slug>`.
    pub fn parse(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .ok_or_else(|| format!("{url:?} is not an http(s) URL"))?;
        let mut parts = rest.trim_end_matches('/').split('/');
        let host = GitHubHost::parse(parts.next().unwrap_or_default())?;
        let segs: Vec<&str> = parts.collect();
        let owner = match segs.as_slice() {
            ["enterprises", slug] => Owner::Enterprise(login(slug, url)?),
            [org] => Owner::Org(login(org, url)?),
            [owner, repo] => Owner::Repo {
                owner: login(owner, url)?,
                repo: repo_name(repo, url)?,
            },
            _ => return Err(format!("{url:?} names no org, repository or enterprise")),
        };
        Ok(Self { host, owner })
    }

    /// The org, repository owner or enterprise slug: the `org` everything is grouped by.
    pub fn login(&self) -> &str {
        match &self.owner {
            Owner::Org(o) | Owner::Enterprise(o) => o,
            Owner::Repo { owner, .. } => owner,
        }
    }

    /// API path listing this scope's runners, or why it cannot be listed.
    pub fn runners_path(&self) -> Result<String, &'static str> {
        match &self.owner {
            Owner::Org(o) => Ok(format!("/orgs/{o}/actions/runners")),
            Owner::Repo { owner, repo } => Ok(format!("/repos/{owner}/{repo}/actions/runners")),
            Owner::Enterprise(_) => {
                Err("enterprise runners need a classic token, which ghr-stats does not accept")
            }
        }
    }
}

fn login(s: &str, url: &str) -> Result<String, String> {
    let ok =
        !s.is_empty() && s.len() <= 39 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then(|| s.to_string())
        .ok_or_else(|| format!("{s:?} in {url:?} is not a GitHub login"))
}

fn repo_name(s: &str, url: &str) -> Result<String, String> {
    is_repo_name(s)
        .then(|| s.to_string())
        .ok_or_else(|| format!("{s:?} in {url:?} is not a repository name"))
}

pub(crate) fn is_repo_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Whether `repo` is `<owner>/<name>` for `owner` (case-insensitive).
pub(crate) fn is_repo_of(repo: &str, owner: &str) -> bool {
    repo.split_once('/')
        .is_some_and(|(o, name)| o.eq_ignore_ascii_case(owner) && is_repo_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(url: &str) -> RunnerScope {
        RunnerScope::parse(url).unwrap()
    }

    #[test]
    fn github_urls_parse_into_their_scope() {
        assert_eq!(
            scope("https://github.com/example-org").owner,
            Owner::Org("example-org".into())
        );
        assert_eq!(
            scope("https://github.com/someone/tools/").owner,
            Owner::Repo {
                owner: "someone".into(),
                repo: "tools".into()
            }
        );
        assert_eq!(
            scope("https://github.com/enterprises/acme").owner,
            Owner::Enterprise("acme".into())
        );
        assert_eq!(
            scope("https://GHE.Example.com/eng").host.to_string(),
            "ghe.example.com"
        );
        for bad in [
            "github.com/org",
            "https://github.com/",
            "https://github.com/a/b/c",
            "https://github.com/org name",
            "https://github.com/o/..",
        ] {
            assert!(RunnerScope::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn each_host_kind_has_its_api_root() {
        assert_eq!(GitHubHost::dotcom().api_base(), "https://api.github.com");
        assert_eq!(
            GitHubHost::parse("acme.ghe.com").unwrap().api_base(),
            "https://api.acme.ghe.com"
        );
        assert_eq!(
            GitHubHost::parse("ghe.example.com").unwrap().api_base(),
            "https://ghe.example.com/api/v3"
        );
    }

    #[test]
    fn repo_slugs_must_belong_to_the_owner() {
        assert!(is_repo_of("Example-Org/app.rs", "example-org"));
        assert!(!is_repo_of("other/app", "example-org"));
        assert!(!is_repo_of("example-org/../x", "example-org"));
        assert!(!is_repo_of("example-org", "example-org"));
    }
}
