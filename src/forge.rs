//! Forges: the services that keep review requests, GitHub's pull requests and
//! GitLab's merge requests. Graft lands with plain git on any remote; a forge, when
//! the remote has one Graft knows, adds the review request on top and says when one
//! was merged.
//!
//! Their APIs are reached through `curl`, which brings the system's certificates and
//! proxy settings along. A token comes from the forge's variable (`GITHUB_TOKEN` or
//! `GH_TOKEN`; `GITLAB_TOKEN`), else from its command (`gh auth token`; `glab`), and
//! reaches curl on its stdin, never on its command line. Without one, a public
//! repository can still be read.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Which forge, and where its API is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[serde(rename = "github")]
    GitHub,
    #[serde(rename = "gitlab")]
    GitLab,
}

/// A project's forge, as kept with the project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub kind: Kind,
    /// The repository on the forge: `owner/name` on GitHub, `group/…/name` on GitLab.
    pub project: String,
    /// The forge's host, which says where its API is.
    pub host: String,
}

impl Spec {
    /// The forge a remote URL's host is known to have: github.com and gitlab.com.
    pub fn of_host(host: &str, path: &str) -> Option<Spec> {
        let host = host.to_ascii_lowercase();
        let kind = match host.trim_start_matches("www.") {
            "github.com" => Kind::GitHub,
            "gitlab.com" => Kind::GitLab,
            _ => return None,
        };
        Spec::new(kind, &host, path)
    }

    /// A forge of `kind` on `host` for the repository at `path` there.
    pub fn new(kind: Kind, host: &str, path: &str) -> Option<Spec> {
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        let valid = |part: &&str| {
            !matches!(*part, "" | "." | "..") && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        };
        let fits = match kind {
            Kind::GitHub => parts.len() == 2,
            Kind::GitLab => parts.len() >= 2,
        };
        (fits && parts.iter().all(valid)).then(|| Spec {
            kind,
            project: parts.join("/"),
            host: host.trim_start_matches("www.").to_ascii_lowercase(),
        })
    }

    /// `github`, `gitlab:group/name` and the like, as `--forge` takes them, on `host`
    /// for the repository at `path` there unless the text names one.
    pub fn parse(text: &str, host: Option<&str>, path: Option<&str>) -> Result<Spec> {
        let (kind, project) = text.split_once(':').map_or((text, None), |(kind, project)| (kind, Some(project)));
        let kind = match kind.to_ascii_lowercase().as_str() {
            "github" => Kind::GitHub,
            "gitlab" => Kind::GitLab,
            _ => bail!("--forge takes github or gitlab, then optionally :PROJECT, as in gitlab:group/name"),
        };
        let default_host = match kind {
            Kind::GitHub => "github.com",
            Kind::GitLab => "gitlab.com",
        };
        let host = host.filter(|host| !host.is_empty()).unwrap_or(default_host);
        let path = project.or(path).context("say which project, as in --forge github:OWNER/NAME")?;
        Spec::new(kind, host, path).with_context(|| match kind {
            Kind::GitHub => format!("{path} isn't OWNER/NAME"),
            Kind::GitLab => format!("{path} isn't GROUP/NAME"),
        })
    }
}

/// Whether a review request is open, merged, or closed without being merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    Merged,
    Closed,
}

/// A pull request or a merge request.
#[derive(Clone, Debug)]
pub struct Review {
    pub number: u64,
    pub url: String,
    pub state: State,
    /// The branch it goes against.
    pub base: String,
    /// Its branch's last commit.
    pub head: String,
    /// Once merged, the commit that brought it into its base, when there is one.
    pub merge: Option<String>,
}

pub struct Forge {
    pub kind: Kind,
    /// Where the repository's API calls go: `…/repos/OWNER/NAME` or `…/projects/ID`.
    base: String,
    project: String,
    token: Option<String>,
}

impl Forge {
    pub fn new(spec: &Spec) -> Forge {
        let (variable, default) = match spec.kind {
            Kind::GitHub if spec.host == "github.com" => ("GRAFT_GITHUB_API", "https://api.github.com".to_owned()),
            Kind::GitHub => ("GRAFT_GITHUB_API", format!("https://{}/api/v3", spec.host)),
            Kind::GitLab => ("GRAFT_GITLAB_API", format!("https://{}/api/v4", spec.host)),
        };
        let api = std::env::var(variable).unwrap_or(default);
        let api = api.trim_end_matches('/');
        let base = match spec.kind {
            Kind::GitHub => format!("{api}/repos/{}", spec.project),
            Kind::GitLab => format!("{api}/projects/{}", encode(&spec.project)),
        };
        Forge { kind: spec.kind, base, project: spec.project.clone(), token: token(spec) }
    }

    /// Refuse when there is no token to open or change review requests with.
    pub fn signed_in(&self) -> Result<()> {
        if self.token.is_none() {
            bail!(
                "{} wants a token to open {}s: {} (or `gft get` the repository again with --forge none to \
                 land branches alone)",
                self.name(),
                self.noun(),
                sign_in(self.kind)
            );
        }
        Ok(())
    }

    /// `GitHub` or `GitLab`, for people.
    pub fn name(&self) -> &'static str {
        match self.kind {
            Kind::GitHub => "GitHub",
            Kind::GitLab => "GitLab",
        }
    }

    /// What it calls a review request.
    pub fn noun(&self) -> &'static str {
        match self.kind {
            Kind::GitHub => "pull request",
            Kind::GitLab => "merge request",
        }
    }

    /// Review request `number`.
    pub fn get(&self, number: u64) -> Result<Review> {
        let path = match self.kind {
            Kind::GitHub => format!("pulls/{number}"),
            Kind::GitLab => format!("merge_requests/{number}"),
        };
        self.review(&self.call("GET", &path, None)?)
    }

    /// The open review request from `branch`, if there is one.
    pub fn open_from(&self, branch: &str) -> Result<Option<Review>> {
        let found = self.from(branch, "open")?;
        match found.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one.clone())),
            _ => bail!("{branch} has more than one open {}; close all but one and land again", self.noun()),
        }
    }

    /// The merged review requests from `branch`.
    pub fn merged_from(&self, branch: &str) -> Result<Vec<Review>> {
        Ok(self.from(branch, "merged")?.into_iter().filter(|review| review.state == State::Merged).collect())
    }

    fn from(&self, branch: &str, state: &str) -> Result<Vec<Review>> {
        let path = match (self.kind, state) {
            (Kind::GitHub, "open") => format!("pulls?head={}:{}&state=open", self.owner(), query(branch)),
            (Kind::GitHub, _) => format!("pulls?head={}:{}&state=closed", self.owner(), query(branch)),
            (Kind::GitLab, "open") => format!("merge_requests?source_branch={}&state=opened", query(branch)),
            (Kind::GitLab, _) => format!("merge_requests?source_branch={}&state=merged", query(branch)),
        };
        let found = self.call("GET", &path, None)?;
        found
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            // GitLab matches the branch's name in forks too; only the project's own counts.
            .filter(|one| self.kind != Kind::GitLab || one["source_project_id"] == one["project_id"])
            .map(|one| self.review(one))
            .collect()
    }

    /// Open a review request from `head` against `base`.
    pub fn create(&self, title: &str, head: &str, base: &str, body: &str, draft: bool) -> Result<Review> {
        let made = match self.kind {
            Kind::GitHub => self.call(
                "POST",
                "pulls",
                Some(&json!({"title": title, "head": head, "base": base, "body": body, "draft": draft})),
            )?,
            Kind::GitLab => {
                let title = if draft { format!("Draft: {title}") } else { title.to_owned() };
                self.call(
                    "POST",
                    "merge_requests",
                    Some(&json!({"title": title, "source_branch": head, "target_branch": base,
                        "description": body})),
                )?
            }
        };
        self.review(&made)
    }

    /// Give review request `number` the description `body`.
    pub fn describe(&self, number: u64, body: &str) -> Result<()> {
        match self.kind {
            Kind::GitHub => self.call("PATCH", &format!("pulls/{number}"), Some(&json!({"body": body}))),
            Kind::GitLab => self.call("PUT", &format!("merge_requests/{number}"), Some(&json!({"description": body}))),
        }
        .map(drop)
    }

    /// Have review request `number` go against `base`.
    pub fn retarget(&self, number: u64, base: &str) -> Result<()> {
        match self.kind {
            Kind::GitHub => self.call("PATCH", &format!("pulls/{number}"), Some(&json!({"base": base}))),
            Kind::GitLab => {
                self.call("PUT", &format!("merge_requests/{number}"), Some(&json!({"target_branch": base})))
            }
        }
        .map(drop)
    }

    /// Whether `commit` has `ancestor` in its history, as the forge sees it. A commit
    /// the forge doesn't have has nothing.
    pub fn contains(&self, commit: &str, ancestor: &str) -> Result<bool> {
        match self.kind {
            Kind::GitHub => {
                let compared = self.get_opt(&format!("compare/{ancestor}...{commit}"))?;
                Ok(compared.is_some_and(|compared| matches!(compared["status"].as_str(), Some("identical" | "ahead"))))
            }
            Kind::GitLab => {
                let found = self.get_opt(&format!("repository/merge_base?refs[]={ancestor}&refs[]={commit}"))?;
                Ok(found.is_some_and(|found| found["id"] == ancestor))
            }
        }
    }

    fn owner(&self) -> &str {
        self.project.split('/').next().unwrap_or_default()
    }

    fn review(&self, value: &Value) -> Result<Review> {
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
        let some = |key: &str| value[key].as_str().filter(|text| !text.is_empty()).map(str::to_owned);
        Ok(match self.kind {
            Kind::GitHub => Review {
                number: value["number"].as_u64().context("GitHub gave no pull request number")?,
                url: text("html_url"),
                state: match (value["state"].as_str(), value["merged_at"].is_string() || value["merged"] == true) {
                    (_, true) => State::Merged,
                    (Some("open"), _) => State::Open,
                    _ => State::Closed,
                },
                base: value["base"]["ref"].as_str().unwrap_or_default().to_owned(),
                head: value["head"]["sha"].as_str().unwrap_or_default().to_owned(),
                merge: some("merge_commit_sha"),
            },
            Kind::GitLab => Review {
                number: value["iid"].as_u64().context("GitLab gave no merge request number")?,
                url: text("web_url"),
                state: match value["state"].as_str() {
                    Some("opened") => State::Open,
                    Some("merged") => State::Merged,
                    _ => State::Closed,
                },
                base: text("target_branch"),
                head: text("sha"),
                // A fast-forward merge leaves the branch's own commit as the merge.
                merge: some("merge_commit_sha").or_else(|| some("squash_commit_sha")).or_else(|| some("sha")),
            },
        })
    }

    /// Call `method` on `path` under the repository, as `pulls` or `merge_requests`.
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        match self.try_call(method, path, body)? {
            (status, value) if status < 300 => Ok(value),
            (status, value) => bail!("{} answered {method} {path} with {status}: {}", self.name(), message(&value)),
        }
    }

    /// Like [`Forge::call`] for a GET, but none for a 404.
    fn get_opt(&self, path: &str) -> Result<Option<Value>> {
        match self.try_call("GET", path, None)? {
            (404, _) => Ok(None),
            (status, value) if status < 300 => Ok(Some(value)),
            (status, value) => bail!("{} answered GET {path} with {status}: {}", self.name(), message(&value)),
        }
    }

    fn try_call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let url = format!("{}/{path}", self.base);
        let body_file = body.map(|body| tempfile_with(&serde_json::to_vec(body)?)).transpose()?;
        // Everything curl is told comes on its stdin as a config file, so the token
        // never shows in the process list.
        let mut config = format!(
            "url = \"{url}\"\nrequest = \"{method}\"\nsilent\nshow-error\nheader = \"Expect:\"\n\
             write-out = \"\\n%{{http_code}}\"\n"
        );
        if let Some(token) = &self.token {
            config.push_str(&format!("header = \"Authorization: Bearer {token}\"\n"));
        }
        if self.kind == Kind::GitHub {
            config.push_str(
                "header = \"Accept: application/vnd.github+json\"\nheader = \"X-GitHub-Api-Version: 2022-11-28\"\n",
            );
        }
        if let Some(file) = &body_file {
            config.push_str(&format!(
                "header = \"Content-Type: application/json\"\ndata-binary = \"@{}\"\n",
                file.display()
            ));
        }
        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running curl")?;
        child.stdin.take().expect("piped").write_all(config.as_bytes())?;
        let out = child.wait_with_output()?;
        if let Some(file) = body_file {
            let _ = std::fs::remove_file(file);
        }
        if !out.status.success() {
            bail!("reaching {}: {}", self.name(), String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (json, status) = text.rsplit_once('\n').unwrap_or(("", &text));
        let status: u16 =
            status.trim().parse().with_context(|| format!("unexpected answer from {}: {text}", self.name()))?;
        let value = if json.trim().is_empty() { Value::Null } else { serde_json::from_str(json)? };
        if status == 401 && self.token.is_none() {
            bail!("{} wants a token: {}", self.name(), sign_in(self.kind));
        }
        Ok((status, value))
    }
}

/// How to give Graft a token for a forge of `kind`.
pub fn sign_in(kind: Kind) -> &'static str {
    match kind {
        Kind::GitHub => "set GITHUB_TOKEN, or sign in with `gh auth login`",
        Kind::GitLab => "set GITLAB_TOKEN, or sign in with `glab auth login`",
    }
}

fn message(value: &Value) -> String {
    match &value["message"] {
        Value::String(text) => text.clone(),
        Value::Null => value["error"].as_str().unwrap_or("no message").to_owned(),
        other => other.to_string(),
    }
}

/// The token for a forge, if one can be had.
pub fn token(spec: &Spec) -> Option<String> {
    let (variables, command): (&[&str], Vec<&str>) = match spec.kind {
        Kind::GitHub => (&["GITHUB_TOKEN", "GH_TOKEN"], vec!["gh", "auth", "token", "--hostname", &spec.host]),
        Kind::GitLab => (&["GITLAB_TOKEN"], vec!["glab", "config", "get", "token", "--host", &spec.host]),
    };
    if let Some(token) = variables.iter().find_map(|name| std::env::var(name).ok()).filter(|token| !token.is_empty()) {
        return Some(token);
    }
    let out = Command::new(command[0]).args(&command[1..]).stdin(Stdio::null()).stderr(Stdio::null()).output();
    out.ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|token| !token.is_empty())
}

/// `text` with everything but letters, digits and `-_.~` percent-encoded.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// `text` as a query value: encoded, but for the slashes branch names have.
fn query(text: &str) -> String {
    encode(text).replace("%2F", "/")
}

/// A new file in the temporary folder holding `data`.
fn tempfile_with(data: &[u8]) -> Result<std::path::PathBuf> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("graft-{}-{n}.json", std::process::id()));
    std::fs::write(&path, data).context("writing a request body")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forges_come_from_hosts_and_flags() {
        let github = Spec::of_host("github.com", "/lydakis/graft.git").unwrap();
        assert_eq!((github.kind, github.project.as_str()), (Kind::GitHub, "lydakis/graft"));
        let gitlab = Spec::of_host("gitlab.com", "group/sub/name").unwrap();
        assert_eq!((gitlab.kind, gitlab.project.as_str()), (Kind::GitLab, "group/sub/name"));
        assert!(Spec::of_host("example.com", "a/b").is_none());
        assert!(Spec::of_host("github.com", "a/b/c").is_none());
        let flag = Spec::parse("gitlab", Some("git.example.com"), Some("team/app")).unwrap();
        assert_eq!((flag.host.as_str(), flag.project.as_str()), ("git.example.com", "team/app"));
        let named = Spec::parse("github:o/n", None, Some("ignored/path/here")).unwrap();
        assert_eq!((named.host.as_str(), named.project.as_str()), ("github.com", "o/n"));
        assert!(Spec::parse("gitea", None, Some("a/b")).is_err());
        assert_eq!(encode("group/sub name"), "group%2Fsub%20name");
    }
}
