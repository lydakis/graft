//! The programs Graft drives: `sandfs` for the files and `git` for reading commits.

use std::collections::HashSet;
use std::io::Write;
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

/// The files the workspace `name` changed, or how it differs from `since`, leaving out
/// what git ignores in the copy at `dir`, such as build output: those are no change
/// anyone means to land.
pub fn changes(name: &str, since: Option<&str>, dir: &Path) -> Result<Vec<Change>> {
    let mut args = vec!["changes", name];
    if let Some(since) = since {
        args.extend(["--since", since]);
    }
    let (out, _) = sandfs(&args)?;
    let all = out
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once(' ') {
            Some((kind, path)) if kind.len() == 1 => {
                Ok(Change { kind: kind.chars().next().expect("one char"), path: path.to_owned() })
            }
            _ => bail!("unexpected line from sandfs changes: {line:?}"),
        })
        .collect::<Result<Vec<_>>>()?;
    let ignored = ignored(dir, all.iter().map(|change| change.path.as_str()))?;
    Ok(all.into_iter().filter(|change| !ignored.contains(&change.path)).collect())
}

/// Which of `paths` git ignores in the copy at `dir`. Files git tracks never are. A
/// copy that isn't mounted can't say, so nothing is.
fn ignored<'a>(dir: &Path, paths: impl Iterator<Item = &'a str>) -> Result<HashSet<String>> {
    let input: Vec<u8> = paths.flat_map(|path| path.bytes().chain([0])).collect();
    if input.is_empty() || !dir.join(".git").exists() {
        return Ok(HashSet::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "--stdin", "-z"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    let mut stdin = child.stdin.take().expect("piped");
    // Written from a thread of its own, so a long answer can't block the writing.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().context("running git check-ignore")?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("writing to git check-ignore"))?
        .context("writing to git check-ignore")?;
    // 1: nothing is ignored.
    if !output.status.success() && output.status.code() != Some(1) {
        bail!("git check-ignore failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
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
