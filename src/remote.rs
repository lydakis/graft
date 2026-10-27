//! The remote a project lands on, reached with plain git, so landing works wherever
//! git can push: GitHub, GitLab, any other host, or a bare repository over ssh.
//!
//! A landing's commit is made in the copy itself, from its files, with a temporary
//! index laid over the base commit's tree: no checkout of the history, and nothing a
//! copy's own git state (its index, branch or commits) is changed by. Pushes go with
//! a lease on the branch, so one never replaces commits it didn't expect there.
//!
//! git asks for credentials as it always does (ssh keys, a credential helper). When
//! the project's forge has a token, git is offered it after its own helpers, so a
//! machine signed in to `gh` or `glab` alone can push too. git never prompts.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::forge::{self, Kind, Spec};
use crate::state::Project;
use crate::tools::Change;

/// Where a project's copies land.
pub struct Remote {
    pub url: String,
    /// A forge's user name and token for https, offered after git's own credentials.
    auth: Option<(&'static str, String)>,
}

impl Remote {
    pub fn of(project: &Project) -> Remote {
        Remote::new(&project.remote, project.forge.as_ref())
    }

    pub fn new(url: &str, forge: Option<&Spec>) -> Remote {
        let https = url.starts_with("https://") || url.starts_with("http://");
        let auth =
            forge.filter(|forge| https && host_of(url).is_some_and(|host| host == forge.host)).and_then(|spec| {
                let user = match spec.kind {
                    Kind::GitHub => "x-access-token",
                    Kind::GitLab => "oauth2",
                };
                forge::token(spec).map(|token| (user, token))
            });
        Remote { url: url.to_owned(), auth }
    }

    /// git in `dir` for talking to the remote.
    fn git(&self, dir: &Path) -> Command {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null());
        if let Some((user, token)) = &self.auth {
            // The helper reads the token from its environment: it never shows in the
            // process list or lands in any config file.
            command
                .args([
                    "-c",
                    "credential.helper=!f() { test \"$1\" = get || exit 0; echo \"username=$GRAFT_GIT_USER\"; \
                         echo \"password=$GRAFT_GIT_TOKEN\"; }; f",
                ])
                .env("GRAFT_GIT_USER", user)
                .env("GRAFT_GIT_TOKEN", token);
        }
        command
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Result<std::process::Output> {
        let output = self.git(dir).args(args).output().context("running git")?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args.first().copied().unwrap_or_default(), said.trim());
        }
        Ok(output)
    }

    /// The remote's branches whose names match `pattern` (a glob, as `graft/*`), with
    /// the commit each is at.
    pub fn heads(&self, dir: &Path, pattern: &str) -> Result<BTreeMap<String, String>> {
        let refs = format!("refs/heads/{pattern}");
        let output = self.run(dir, &["ls-remote", "--heads", &self.url, &refs])?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (commit, name) = line.split_once('\t')?;
                Some((name.strip_prefix("refs/heads/")?.to_owned(), commit.to_owned()))
            })
            .collect())
    }

    /// The commit the branch `branch` is at, if it exists.
    pub fn head(&self, dir: &Path, branch: &str) -> Result<Option<String>> {
        Ok(self.heads(dir, branch)?.remove(branch))
    }

    /// The remote's default branch.
    pub fn default_branch(&self, dir: &Path) -> Result<String> {
        let output = self.run(dir, &["ls-remote", "--symref", &self.url, "HEAD"])?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("ref: refs/heads/")?.split_once('\t').map(|(name, _)| name.to_owned()))
            .with_context(|| format!("{} names no default branch", self.url))
    }

    /// Put `commit` on the branch `branch`, only if the branch is at `expected` (none:
    /// only if there is no such branch). What the remote printed for people, such as a
    /// link to open a review request, is returned.
    pub fn push(&self, dir: &Path, commit: &str, branch: &str, expected: Option<&str>) -> Result<Vec<String>> {
        let target = format!("refs/heads/{branch}");
        let lease = format!("--force-with-lease={target}:{}", expected.unwrap_or_default());
        let output = self.run(dir, &["push", "--no-verify", &lease, &self.url, &format!("{commit}:{target}")])?;
        Ok(said(&output.stderr))
    }

    /// Delete the branch `branch`, only if it is at `expected`. False when it isn't.
    pub fn delete(&self, dir: &Path, branch: &str, expected: &str) -> Result<bool> {
        let target = format!("refs/heads/{branch}");
        let lease = format!("--force-with-lease={target}:{expected}");
        let output = self.git(dir).args(["push", "--no-verify", &lease, &self.url, &format!(":{target}")]).output()?;
        match output.status.success() {
            true => Ok(true),
            false if String::from_utf8_lossy(&output.stderr).contains("stale info") => Ok(false),
            false => bail!("deleting {branch}: {}", String::from_utf8_lossy(&output.stderr).trim()),
        }
    }

    /// Fetch `commits` into the copy at `dir`, each alone, without its history or its
    /// files, unless the copy has them already: a commit the landing builds on, so a
    /// push of the landing doesn't walk back through a history the copy lacks.
    pub fn fetch_commits(&self, dir: &Path, commits: &[&str]) -> Result<()> {
        let missing: Vec<&str> = commits.iter().copied().filter(|commit| !has(dir, commit)).collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut args = vec!["fetch", "-q", "--depth=1", "--filter=blob:none", "--no-tags", "--no-write-fetch-head"];
        args.push("--recurse-submodules=no");
        args.push(&self.url);
        args.extend(missing);
        self.run(dir, &args).map(drop)
    }
}

/// The remote's lines a push printed for people (`remote: …`), such as a link to open
/// a merge request.
fn said(stderr: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter_map(|line| line.strip_prefix("remote:"))
        .map(str::trim)
        .filter(|line| line.contains("://"))
        .map(str::to_owned)
        .collect()
}

/// Whether the copy at `dir` has the commit `commit`, without fetching it: a partial
/// clone fetches what it lacks when asked for it, and slowly.
fn has(dir: &Path, commit: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-list", "-n1", "--no-walk", "--missing=print", commit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The host of a remote URL: `https://host/…`, `ssh://user@host:port/…`, `user@host:…`.
pub fn host_of(url: &str) -> Option<String> {
    let authority = match url.split_once("://") {
        Some((_, rest)) => rest.split('/').next()?,
        None => url.split_once(':')?.0,
    };
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The id of the tree of `commit`, as the copy at `dir` has it or can fetch it.
pub fn tree_of(dir: &Path, commit: &str) -> Result<String> {
    crate::tools::git(dir, &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// Make a commit in the copy at `dir` of its files: `base`'s tree with the files in
/// `changed` as they are now, on `parents`. Returns the commit and its tree.
pub fn commit(
    dir: &Path,
    base: &str,
    changed: &[Change],
    parents: &[String],
    message: &str,
) -> Result<(String, String)> {
    let index = temp_path("index");
    let result = commit_with(dir, &index, base, changed, parents, message);
    let _ = std::fs::remove_file(&index);
    result
}

fn commit_with(
    dir: &Path,
    index: &Path,
    base: &str,
    changed: &[Change],
    parents: &[String],
    message: &str,
) -> Result<(String, String)> {
    // Who made it, when git in the copy can't say.
    let anonymous = crate::tools::git(dir, &["config", "user.email"]).is_err();
    let git = |args: &[&str], input: Option<&[u8]>| -> Result<String> {
        let mut command = Command::new("git");
        command.arg("-C").arg(dir).args(args).env("GIT_INDEX_FILE", index).env("GIT_TERMINAL_PROMPT", "0");
        if anonymous {
            for variable in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
                command.env(variable, "Graft");
            }
            for variable in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
                command.env(variable, "graft@localhost");
            }
        }
        let mut child = command
            .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running git")?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("piped");
            let input = input.to_vec();
            let writer = std::thread::spawn(move || stdin.write_all(&input));
            let output = child.wait_with_output()?;
            writer.join().map_err(|_| anyhow::anyhow!("writing to git"))??;
            return checked(args, output);
        }
        checked(args, child.wait_with_output()?)
    };
    git(&["read-tree", base], None)?;
    // Files, by path, as git hashes them: one git for all of them.
    let mut entries: Vec<u8> = Vec::new();
    let mut files: Vec<(&str, &str)> = Vec::new();
    for change in changed {
        if change.kind == 'D' {
            entries.extend_from_slice(format!("0 {}\t{}\0", "0".repeat(40), change.path).as_bytes());
            continue;
        }
        let (mode, link) = crate::land::mode(&dir.join(&change.path))?;
        match link {
            Some(target) => {
                let id = git(&["hash-object", "-w", "--stdin"], Some(&target))?;
                entries.extend_from_slice(format!("{mode} {id}\t{}\0", change.path).as_bytes());
            }
            None => files.push((mode, &change.path)),
        }
    }
    if !files.is_empty() {
        let paths: Vec<u8> = files.iter().flat_map(|(_, path)| path.bytes().chain(*b"\n")).collect();
        if files.iter().any(|(_, path)| path.contains('\n')) {
            bail!("a file name with a line break in it can't be landed");
        }
        let ids = git(&["hash-object", "-w", "--no-filters", "--stdin-paths"], Some(&paths))?;
        for ((mode, path), id) in files.iter().zip(ids.lines()) {
            entries.extend_from_slice(format!("{mode} {id}\t{path}\0").as_bytes());
        }
    }
    git(&["update-index", "-z", "--index-info"], Some(&entries))?;
    let tree = git(&["write-tree"], None)?;
    let mut args = vec!["commit-tree", tree.as_str()];
    for parent in parents {
        args.extend(["-p", parent.as_str()]);
    }
    let commit = git(&args, Some(message.as_bytes()))?;
    Ok((commit, tree))
}

fn checked(args: &[&str], output: std::process::Output) -> Result<String> {
    if !output.status.success() {
        bail!("git {} failed: {}", args[0], String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// A path in the temporary folder no other Graft uses.
fn temp_path(what: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("graft-{what}-{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_of_remote_urls() {
        assert_eq!(host_of("https://github.com/o/n").as_deref(), Some("github.com"));
        assert_eq!(host_of("ssh://git@Git.Example.com:2222/o/n.git").as_deref(), Some("git.example.com"));
        assert_eq!(host_of("git@gitlab.com:group/name.git").as_deref(), Some("gitlab.com"));
        assert_eq!(host_of("file:///srv/repo").as_deref(), None);
    }

    #[test]
    fn remote_lines_with_links_are_kept() {
        let said = said(b"remote: \nremote: To create a merge request for x, visit:\nremote:   https://gitlab.com/g/n/-/merge_requests/new?x\nTo https://gitlab.com/g/n\n");
        assert_eq!(said, ["https://gitlab.com/g/n/-/merge_requests/new?x"]);
    }
}
