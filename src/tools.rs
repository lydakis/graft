//! The programs Graft drives: `sandfs` for the files and `git` for reading commits.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

/// The `sandfs` command: `$GRAFT_SANDFS`, else `sandfs` on the PATH.
fn sandfs_command() -> Command {
    Command::new(std::env::var_os("GRAFT_SANDFS").unwrap_or_else(|| "sandfs".into()))
}

fn check(what: &str, output: Output) -> Result<Output> {
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim().trim_start_matches("Error: ");
        bail!("{what} failed: {said}");
    }
    Ok(output)
}

/// Run `sandfs` with `args` and return what it printed on stdout and stderr.
pub fn sandfs(args: &[&str]) -> Result<(String, String)> {
    let output = sandfs_command()
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running sandfs; install it from https://github.com/lydakis/sandfs")?;
    let output = check(&format!("sandfs {}", args.first().copied().unwrap_or("")), output)?;
    Ok((String::from_utf8_lossy(&output.stdout).into_owned(), String::from_utf8_lossy(&output.stderr).into_owned()))
}

/// One line of `sandfs changes`: `A`, `M` or `D`, and the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: char,
    pub path: String,
}

/// The files the workspace `name` changed, or how it differs from `since`.
pub fn changes(name: &str, since: Option<&str>) -> Result<Vec<Change>> {
    let mut args = vec!["changes", name];
    if let Some(since) = since {
        args.extend(["--since", since]);
    }
    let (out, _) = sandfs(&args)?;
    out.lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once(' ') {
            Some((kind, path)) if kind.len() == 1 => {
                Ok(Change { kind: kind.chars().next().expect("one char"), path: path.to_owned() })
            }
            _ => bail!("unexpected line from sandfs changes: {line:?}"),
        })
        .collect()
}

/// Where the workspace `name` is mounted, if it is.
pub fn mounted(name: &str) -> Result<Option<PathBuf>> {
    let (out, _) = sandfs(&["list"])?;
    Ok(out.lines().find_map(|line| {
        let mut fields = line.split('\t');
        (fields.next() == Some(name)).then(|| fields.nth(1)).flatten().filter(|dir| *dir != "-").map(PathBuf::from)
    }))
}

/// Mount the workspace `name` at `dir` unless it is mounted already, as after a reboot.
pub fn ensure_mounted(name: &str, dir: &Path) -> Result<()> {
    if mounted(name)?.is_none() {
        sandfs(&["mount", name, path_str(dir)?])?;
    }
    Ok(())
}

pub fn path_str(path: &Path) -> Result<&str> {
    path.to_str().with_context(|| format!("{} is not UTF-8", path.display()))
}

/// Run git in `dir` and return what it printed, trimmed.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_raw(dir, args)?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

/// Run git in `dir` and return what it printed, as bytes.
pub fn git_raw(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    Ok(check(&format!("git {}", args.join(" ")), output)?.stdout)
}

/// A commit id cut to the length people read.
pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(10)]
}
