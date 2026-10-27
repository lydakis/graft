//! What Graft drives: sandfs, as a library, for the files and `git` for reading commits.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};
use sandfs::{ChangeKind, ChangesOptions, ForkOptions, MountOptions};

fn check(what: &str, output: Output) -> Result<Output> {
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let said = said.trim().trim_start_matches("Error: ");
        bail!("{what} failed: {said}");
    }
    Ok(output)
}

/// sandfs's workspaces: `$SANDFS_HOME`, else `$XDG_DATA_HOME/sandfs`, else
/// `~/.local/share/sandfs`, the same ones the `sandfs` command sees.
pub fn sandfs() -> Result<sandfs::Home> {
    sandfs::Home::locate()
}

/// Mount a new workspace `name` over `source` at `dir`.
pub fn mount(source: &str, dir: &Path, name: &str) -> Result<()> {
    let options = MountOptions { name: Some(name.to_owned()), ..Default::default() };
    sandfs()?.mount(source, dir, &options)?;
    Ok(())
}

/// Fork `from` as `name`, mounted at `dir`, or kept unmounted without one.
pub fn fork(from: &str, dir: Option<&Path>, name: &str) -> Result<()> {
    let options = ForkOptions { name: Some(name.to_owned()), ..Default::default() };
    sandfs()?.fork(from, dir, &options)?;
    Ok(())
}

/// Write how `name` differs from `since` (or its source) into the folder `to`.
pub fn copy_changes(name: &str, since: Option<&str>, to: &Path) -> Result<()> {
    let options =
        ChangesOptions { since: since.map(str::to_owned), copy_to: Some(to.to_owned()), ..Default::default() };
    sandfs()?.changes(name, &options)?;
    Ok(())
}

/// Whether the workspaces `a` and `b` hold the same files, build output included.
pub fn same(a: &str, b: &str) -> Result<bool> {
    let options = ChangesOptions { since: Some(b.to_owned()), ..Default::default() };
    Ok(sandfs()?.changes(a, &options)?.changes.is_empty())
}

/// A changed file: `A`, `M` or `D`, and the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub kind: char,
    pub path: String,
}

/// The files the workspace `name` changed, or how it differs from `since`, leaving out
/// what git ignores in the copy at `dir`, such as build output: those are no change
/// anyone means to land.
pub fn changes(name: &str, since: Option<&str>, dir: &Path) -> Result<Vec<Change>> {
    let options = ChangesOptions { since: since.map(str::to_owned), ..Default::default() };
    let all: Vec<Change> = sandfs()?
        .changes(name, &options)?
        .changes
        .into_iter()
        .map(|change| Change {
            kind: match change.kind {
                ChangeKind::Added => 'A',
                ChangeKind::Modified => 'M',
                ChangeKind::Deleted => 'D',
            },
            path: change.path,
        })
        .collect();
    // Against the base, only an added file can be one git ignores: the others are the
    // base commit's own files, whatever the index says after `git rm`. Between copies,
    // either may have build output the other lacks. A path below a link, as a deleted
    // file is when a link replaced its folder, is no file git could ignore, and git
    // refuses to look past the link.
    let mut links = HashMap::new();
    let mut below_link = |path: &str| {
        Path::new(path).ancestors().skip(1).filter(|parent| !parent.as_os_str().is_empty()).any(|parent| {
            *links.entry(parent.to_path_buf()).or_insert_with(|| {
                std::fs::symlink_metadata(dir.join(parent)).is_ok_and(|meta| meta.file_type().is_symlink())
            })
        })
    };
    let asked: Vec<&str> = all
        .iter()
        .filter(|change| since.is_some() || change.kind == 'A')
        .map(|change| change.path.as_str())
        .filter(|path| !below_link(path))
        .collect();
    let ignored = ignored(dir, asked.into_iter())?;
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
    Ok(sandfs()?.workspaces()?.into_iter().find(|workspace| workspace.name == name).and_then(|ws| ws.mounted))
}

/// Mount the workspace `name` at `dir` unless it is mounted already, as after a reboot.
pub fn ensure_mounted(name: &str, dir: &Path) -> Result<()> {
    if mounted(name)?.is_none() {
        sandfs()?.mount(name, dir, &MountOptions::default())?;
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
// bench
