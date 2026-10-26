//! GitHub's REST API, through `curl`, which brings the system's certificates and
//! proxy settings along. The token comes from `GITHUB_TOKEN` or `GH_TOKEN`, else from
//! `gh auth token`, and reaches curl on its stdin, never on its command line.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub struct GitHub {
    /// `$GRAFT_GITHUB_API`, else `https://api.github.com`.
    api: String,
    /// `owner/name`.
    repo: String,
    token: String,
}

impl GitHub {
    pub fn new(repo: &str) -> Result<GitHub> {
        let api = std::env::var("GRAFT_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".to_owned());
        let token = match std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
            Ok(token) => token,
            Err(_) => {
                let out = Command::new("gh").args(["auth", "token"]).stdin(Stdio::null()).output();
                match out {
                    Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
                    _ => bail!("no GitHub token: set GITHUB_TOKEN, or sign in with `gh auth login`"),
                }
            }
        };
        Ok(GitHub { api: api.trim_end_matches('/').to_owned(), repo: repo.to_owned(), token })
    }

    /// Call `method` on `path` under the repository (`git/blobs`, `pulls`, ...).
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        match self.try_call(method, path, body)? {
            (status, value) if status < 300 => Ok(value),
            (status, value) => {
                let message = value.get("message").and_then(Value::as_str).unwrap_or("no message");
                bail!("GitHub answered {method} {path} with {status}: {message}")
            }
        }
    }

    /// Like [`GitHub::call`], but none for a 404.
    pub fn get_opt(&self, path: &str) -> Result<Option<Value>> {
        match self.try_call("GET", path, None)? {
            (404, _) => Ok(None),
            (status, value) if status < 300 => Ok(Some(value)),
            (status, value) => {
                let message = value.get("message").and_then(Value::as_str).unwrap_or("no message");
                bail!("GitHub answered GET {path} with {status}: {message}")
            }
        }
    }

    fn try_call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let url = format!("{}/repos/{}/{}", self.api, self.repo, path);
        let body_file = match body {
            Some(body) => {
                let file = tempfile_with(&serde_json::to_vec(body)?)?;
                Some(file)
            }
            None => None,
        };
        // Everything curl is told comes on its stdin as a config file, so the token
        // never shows in the process list.
        let mut config = format!(
            "url = \"{url}\"\nrequest = \"{method}\"\nsilent\nshow-error\n\
             header = \"Authorization: Bearer {}\"\nheader = \"Accept: application/vnd.github+json\"\n\
             header = \"X-GitHub-Api-Version: 2022-11-28\"\nheader = \"Expect:\"\nwrite-out = \"\\n%{{http_code}}\"\n",
            self.token
        );
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
            bail!("reaching GitHub: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (json, status) = text.rsplit_once('\n').unwrap_or(("", &text));
        let status: u16 = status.trim().parse().with_context(|| format!("unexpected answer from GitHub: {text}"))?;
        let value = if json.trim().is_empty() { Value::Null } else { serde_json::from_str(json)? };
        Ok((status, value))
    }
}

/// A new file in the temporary folder holding `data`.
fn tempfile_with(data: &[u8]) -> Result<std::path::PathBuf> {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("graft-{}-{n}.json", std::process::id()));
    std::fs::write(&path, data).context("writing a request body")?;
    Ok(path)
}
