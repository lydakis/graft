//! Graft (`gft`): work in instant copies of a GitHub repository.
//!
//! Every copy is a sandfs folder over a commit of the repository's default branch, so
//! nothing is cloned and a copy costs what it changes. GitHub stays the place where
//! history, review and merging live: `gft land` turns a copy's changes into a branch
//! and a pull request through GitHub's API, without a local commit or a push.
//!
//! Graft keeps only names and pointers (see `state`); sandfs keeps the files and git
//! inside each copy keeps working as in any checkout.

mod github;
mod land;
mod state;
mod tools;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::state::{Copy, Project, Snapshot, Store, check_copy_name};
use crate::tools::{changes, ensure_mounted, git, mounted, path_str, sandfs, short};

#[derive(Parser)]
#[command(name = "gft", version, about = "Work in instant copies of a GitHub repository, built on sandfs")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount a repository's default branch at DIR (default: ./NAME), downloading
    /// nothing up front. This is the home copy, which other copies start from.
    Get {
        /// OWNER/NAME on GitHub, or the repository's URL.
        repo: String,
        dir: Option<PathBuf>,
        /// Where copies land, as OWNER/NAME, for a URL that isn't github.com's.
        #[arg(long, value_name = "OWNER/NAME")]
        github: Option<String>,
    },
    /// Make copies named NAME on the newest commit of the default branch, or on another
    /// copy with its changes so far. Each appears next to the home copy, as HOME+NAME.
    New {
        #[arg(required = true)]
        names: Vec<String>,
        /// Start from this copy instead, with its changes.
        #[arg(long, value_name = "COPY")]
        from: Option<String>,
    },
    /// List the copies: how many files each changed, the commit it sits on, its pull
    /// request, and which other copies changed the same files.
    Ls,
    /// Print the folder of a copy.
    Path { name: String },
    /// List the files a copy changed (A, M, D), or how copy A differs from copy B.
    Changes { a: Option<String>, b: Option<String> },
    /// Keep a snapshot of the copy as it is now, which `undo` goes back to.
    Save {
        /// What the snapshot is, for `gft ls`.
        note: Vec<String>,
        /// The copy (default: the one the current folder is in).
        #[arg(long, short)]
        copy: Option<String>,
    },
    /// Put the copy back as it was N snapshots ago (default 1, the last one).
    Undo {
        #[arg(default_value_t = 1)]
        steps: usize,
        #[arg(long, short)]
        copy: Option<String>,
    },
    /// Move a copy onto the newest commit of the default branch, keeping its changes.
    /// Files changed on both sides are merged line by line; conflicts are marked in them.
    Sync { name: Option<String> },
    /// Land a copy as a pull request: a branch graft/NAME on GitHub with one commit of
    /// its changes. Landing again adds a commit to the same pull request.
    Land {
        name: Option<String>,
        /// The pull request's title, and the commit's message.
        #[arg(long, short)]
        message: Option<String>,
        /// Open the pull request as a draft.
        #[arg(long)]
        draft: bool,
    },
    /// Delete copies, with their snapshots. Their pull requests stay as they are.
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
    },
}

pub fn main() {
    if let Err(err) = run(Cli::parse()) {
        eprintln!("gft: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let store = Store::locate()?;
    match cli.command {
        Cmd::Get { repo, dir, github } => get(&store, &repo, dir, github),
        Cmd::New { names, from } => new(&store, &names, from.as_deref()),
        Cmd::Ls => ls(&store),
        Cmd::Path { name } => {
            let (project, _) = here(&store)?;
            println!("{}", copy(&project, &name)?.dir.display());
            Ok(())
        }
        Cmd::Changes { a, b } => {
            let (project, current) = here(&store)?;
            let workspace = |name: Option<String>| -> Result<(String, PathBuf)> {
                match name.or(current.clone()) {
                    Some(name) => {
                        let copy = copy(&project, &name)?;
                        Ok((copy.workspace.clone(), copy.dir.clone()))
                    }
                    None => Ok((project.workspace.clone(), project.home.clone())),
                }
            };
            let (a, dir) = workspace(a)?;
            let b = b.map(|b| workspace(Some(b)).map(|(b, _)| b)).transpose()?;
            for change in changes(&a, b.as_deref(), &dir)? {
                println!("{} {}", change.kind, change.path);
            }
            Ok(())
        }
        Cmd::Save { note, copy: name } => {
            let (project, current) = here(&store)?;
            let name = which(name, current)?;
            let snapshot = save(&store, &project.repo, &name, &note.join(" "))?;
            println!("saved {name} as snapshot {}", snapshot.workspace);
            Ok(())
        }
        Cmd::Undo { steps, copy: name } => {
            let (project, current) = here(&store)?;
            leave_mounts()?;
            undo(&store, &project.repo, &which(name, current)?, steps)
        }
        Cmd::Sync { name } => {
            let (project, current) = here(&store)?;
            leave_mounts()?;
            sync(&store, &project.repo, &which(name, current)?)
        }
        Cmd::Land { name, message, draft } => {
            let (project, current) = here(&store)?;
            land::land(&store, &project.repo, &which(name, current)?, message.as_deref(), draft)
        }
        Cmd::Rm { names } => {
            let (project, _) = here(&store)?;
            leave_mounts()?;
            for name in &names {
                rm(&store, &project.repo, name)?;
            }
            Ok(())
        }
    }
}

/// The project the current folder is in, and the copy.
fn here(store: &Store) -> Result<(Project, Option<String>)> {
    store.find(&std::env::current_dir()?)
}

/// Step out of the current folder, which may be in a copy about to be unmounted: a
/// mount can't go while a program, this one included, works inside it.
fn leave_mounts() -> Result<()> {
    std::env::set_current_dir("/").context("leaving the current folder")
}

/// A copy named on the command line, else the one the current folder is in.
fn which(name: Option<String>, current: Option<String>) -> Result<String> {
    name.or(current).context("name a copy, or run this inside one")
}

fn copy<'a>(project: &'a Project, name: &str) -> Result<&'a Copy> {
    project.copies.get(name).with_context(|| format!("{} has no copy {name:?}; `gft ls` lists them", project.repo))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// The workspace that holds the default branch as last fetched, which copies fork.
fn main_workspace(project: &Project) -> String {
    format!("{}._main", project.workspace)
}

/// `owner/name` and the URL to mount for what `gft get` was given.
fn parse_repo(repo: &str, github: Option<String>) -> Result<(String, String)> {
    let slug = |text: &str| {
        let text = text.trim_end_matches('/').trim_end_matches(".git");
        let mut parts = text.rsplitn(3, '/');
        match (parts.next(), parts.next()) {
            (Some(name), Some(owner)) if !name.is_empty() && !owner.is_empty() => {
                Some(format!("{}/{name}", owner.rsplit(':').next().unwrap_or(owner)))
            }
            _ => None,
        }
    };
    if repo.contains("://") || repo.contains('@') {
        let on_github = repo.contains("github.com");
        let slug = match github {
            Some(slug) => slug,
            None if on_github => slug(repo).context("can't tell the repository's owner and name")?,
            None => bail!("{repo} isn't on github.com; say where copies land with --github OWNER/NAME"),
        };
        return Ok((slug, repo.to_owned()));
    }
    let slug = slug(repo).filter(|_| repo.matches('/').count() == 1).context("give the repository as OWNER/NAME")?;
    let url = format!("https://github.com/{slug}");
    Ok((github.unwrap_or(slug), url))
}

fn get(store: &Store, repo: &str, dir: Option<PathBuf>, github: Option<String>) -> Result<()> {
    let (slug, source) = parse_repo(repo, github)?;
    let name = slug.rsplit('/').next().expect("owner/name").to_owned();
    let dir = std::path::absolute(dir.unwrap_or_else(|| PathBuf::from(&name)))?;
    if store.exists(&slug) {
        bail!("{slug} is already a project here; its home copy is listed by `gft ls`");
    }
    sandfs(&["mount", &source, path_str(&dir)?, "--name", &name])?;
    let dir = dir.canonicalize()?;
    let mut project = Project {
        repo: slug.clone(),
        source,
        home: dir.clone(),
        workspace: name.clone(),
        branch: String::new(),
        copies: BTreeMap::new(),
    };
    // Kept before anything changes the home copy, as the start of every new copy.
    sandfs(&["fork", &name, "--name", &main_workspace(&project)])?;
    project.branch = git(&dir, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|_| "main".to_owned());
    let commit = git(&dir, &["rev-parse", "HEAD"])?;
    store.create(&project)?;
    println!("{}: {slug} at {} ({})", dir.display(), short(&commit), project.branch);
    Ok(())
}

fn new(store: &Store, names: &[String], from: Option<&str>) -> Result<()> {
    let (project, _) = here(store)?;
    for name in names {
        check_copy_name(name)?;
        if project.copies.contains_key(name) {
            bail!("{} already has a copy {name:?}", project.repo);
        }
    }
    let (source, mut base) = match from {
        Some(from) => {
            let from = copy(&project, from)?;
            (from.workspace.clone(), Some(from.base.clone()))
        }
        None => {
            // Brings the default branch up to date once for all the copies made now.
            let main = main_workspace(&project);
            sandfs(&["update", &main])?;
            (main, None)
        }
    };
    let parent = project.home.parent().context("the home copy has no parent folder")?;
    let home_name = project.home.file_name().context("the home copy has no folder name")?.to_string_lossy();
    for name in names {
        let dir = parent.join(format!("{home_name}+{name}"));
        let workspace = format!("{}.{name}", project.workspace);
        sandfs(&["fork", &source, path_str(&dir)?, "--name", &workspace])?;
        let commit = match &base {
            Some(commit) => commit.clone(),
            None => git(&dir, &["rev-parse", "HEAD"])?,
        };
        base = Some(commit.clone());
        let copy = Copy {
            dir: dir.canonicalize()?,
            workspace,
            base: commit,
            from: from.map(str::to_owned),
            pr: None,
            snapshots: Vec::new(),
            next: 1,
        };
        store.edit(&project.repo, |project| {
            project.copies.insert(name.clone(), copy);
            Ok(())
        })?;
        println!("{}", dir.display());
    }
    Ok(())
}

fn ls(store: &Store) -> Result<()> {
    let (project, _) = here(store)?;
    println!("{}\t{}\t(home, {})", project.workspace, project.home.display(), project.branch);
    let mut changed: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for (name, copy) in &project.copies {
        let paths = changes(&copy.workspace, None, &copy.dir)?.into_iter().map(|change| change.path).collect();
        changed.insert(name, paths);
    }
    for (name, copy) in &project.copies {
        let mine = &changed[name.as_str()];
        let mut notes = Vec::new();
        for (other, theirs) in &changed {
            if other == name {
                continue;
            }
            let shared: Vec<&String> = mine.intersection(theirs).collect();
            if let Some(first) = shared.first() {
                let more = if shared.len() > 1 { format!(" and {} more", shared.len() - 1) } else { String::new() };
                notes.push(format!("overlaps {other} on {first}{more}"));
            }
        }
        let pr = copy.pr.as_ref().map_or("-".to_owned(), |pr| pr.url.clone());
        let files = match mine.len() {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        };
        println!("{name}\t{files}\tbase {}\t{pr}\t{}", short(&copy.base), notes.join("; "));
    }
    Ok(())
}

fn save(store: &Store, repo: &str, name: &str, note: &str) -> Result<Snapshot> {
    let (workspace, n) = store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.next += 1;
        Ok((copy.workspace.clone(), copy.next - 1))
    })?;
    let snapshot_name = format!("{workspace}.{n}");
    sandfs(&["fork", &workspace, "--name", &snapshot_name])?;
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        let snapshot =
            Snapshot { workspace: snapshot_name, base: copy.base.clone(), note: note.to_owned(), time: now() };
        copy.snapshots.push(snapshot.clone());
        Ok(snapshot)
    })
}

fn undo(store: &Store, repo: &str, name: &str, steps: usize) -> Result<()> {
    let project = store.load(repo)?;
    let copy = copy(&project, name)?;
    if steps == 0 || steps > copy.snapshots.len() {
        bail!("{name} has {} snapshots; `gft save` keeps one", copy.snapshots.len());
    }
    let keep = copy.snapshots.len() - steps;
    let target = copy.snapshots[keep].clone();
    if target.base == copy.base && mounted(&copy.workspace)?.is_some() {
        // Written back through the mount, file by file, so programs working in the
        // copy (a shell sitting in it) carry on undisturbed. Git's own files stay.
        sandfs(&["changes", &target.workspace, "--since", &copy.workspace, "--copy-to", path_str(&copy.dir)?])?;
    } else {
        // A snapshot from before a sync sits on another commit: the copy is made again.
        in_use(copy, sandfs(&["rm", "--unmount", &copy.workspace]))?;
        sandfs(&["fork", &target.workspace, path_str(&copy.dir)?, "--name", &copy.workspace])?;
    }
    // Snapshots newer than the one gone back to go; that one stays, to go back to again.
    for later in &copy.snapshots[keep + 1..] {
        sandfs(&["rm", &later.workspace])?;
    }
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.snapshots.truncate(keep + 1);
        copy.base = target.base.clone();
        Ok(())
    })?;
    let note = if target.note.is_empty() { String::new() } else { format!(" ({})", target.note) };
    println!("{name} is back at snapshot {}{note}", target.workspace);
    Ok(())
}

/// An unmount of `copy` refused because something works in it, said plainly.
fn in_use<T>(copy: &Copy, result: Result<T>) -> Result<T> {
    result.map_err(|err| {
        if format!("{err:#}").contains("busy") {
            anyhow::anyhow!(
                "{} is in use: a shell or program is working in it (its folder or a file in it is open); \
                 leave it, as with `cd ..`, and try again",
                copy.dir.display()
            )
        } else {
            err
        }
    })
}

fn sync(store: &Store, repo: &str, name: &str) -> Result<()> {
    save(store, repo, name, "before sync")?;
    let project = store.load(repo)?;
    let copy = copy(&project, name)?.clone();
    if mounted(&copy.workspace)?.is_some() {
        in_use(&copy, sandfs(&["unmount", &copy.workspace]))?;
    }
    let moved = sandfs(&["update", &copy.workspace]);
    // Mounted again whatever happened, so a refused update leaves the copy as it was.
    sandfs(&["mount", &copy.workspace, path_str(&copy.dir)?])?;
    let (out, _) = moved?;
    let new = git(&copy.dir, &["rev-parse", "HEAD"])?;
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.base = new.clone();
        Ok(())
    })?;
    let both: Vec<&str> = out.lines().filter_map(|line| line.strip_prefix("C ")).collect();
    let mut conflicts = 0;
    for path in &both {
        let outcome = merge(&copy.dir, &copy.base, &new, path)?;
        if outcome != "merged" {
            conflicts += 1;
        }
        println!("{outcome}\t{path}");
    }
    println!(
        "{name} moved from {} to {}; {} files changed on both sides, {conflicts} need a look",
        short(&copy.base),
        short(&new),
        both.len()
    );
    Ok(())
}

/// Merge upstream's change to `path` between commits `old` and `new` into the copy's
/// version in `dir`, line by line, leaving conflict markers where both changed the
/// same lines. Says what happened.
fn merge(dir: &Path, old: &str, new: &str, path: &str) -> Result<&'static str> {
    let mine = dir.join(path);
    let meta = std::fs::symlink_metadata(&mine);
    if !meta.as_ref().is_ok_and(|meta| meta.is_file()) {
        return Ok(if meta.is_err() { "deleted here, changed upstream; kept deleted" } else { "kept yours" });
    }
    let show = |commit: &str| tools::git_raw(dir, &["show", &format!("{commit}:{path}")]).ok();
    let Some(theirs) = show(new) else { return Ok("deleted upstream, changed here; kept yours") };
    let base = show(old).unwrap_or_default();
    let ours = std::fs::read(&mine)?;
    if [&ours, &base, &theirs].iter().any(|data| data.contains(&0)) {
        return Ok("binary, changed on both sides; kept yours");
    }
    let tmp = std::env::temp_dir().join(format!("graft-merge-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let (base_file, theirs_file) = (tmp.join("base"), tmp.join("theirs"));
    std::fs::write(&base_file, &base)?;
    std::fs::write(&theirs_file, &theirs)?;
    let out = std::process::Command::new("git")
        .args(["merge-file", "-p", "-L", "yours", "-L", "base", "-L", "upstream"])
        .args([&mine, &base_file, &theirs_file])
        .output()
        .context("running git merge-file")?;
    let _ = std::fs::remove_dir_all(&tmp);
    // Its exit code is the number of conflicts, negative on failure.
    match out.status.code() {
        Some(code) if code >= 0 => {
            std::fs::write(&mine, &out.stdout)?;
            Ok(if code == 0 { "merged" } else { "conflict" })
        }
        _ => bail!("git merge-file failed on {path}: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

fn rm(store: &Store, repo: &str, name: &str) -> Result<()> {
    let project = store.load(repo)?;
    let copy = copy(&project, name)?;
    if mounted(&copy.workspace)?.is_some() {
        in_use(copy, sandfs(&["rm", "--unmount", &copy.workspace]))?;
    } else {
        sandfs(&["rm", &copy.workspace])?;
    }
    for snapshot in &copy.snapshots {
        sandfs(&["rm", &snapshot.workspace])?;
    }
    store.edit(repo, |project| {
        project.copies.remove(name);
        Ok(())
    })?;
    println!("removed {name}");
    Ok(())
}

/// Make sure the copy is mounted at its folder, as after a reboot.
fn mount_copy(copy: &Copy) -> Result<()> {
    ensure_mounted(&copy.workspace, &copy.dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_are_named_by_owner_and_name() {
        let parse = |repo: &str| parse_repo(repo, None).unwrap();
        assert_eq!(parse("lydakis/sandfs"), ("lydakis/sandfs".into(), "https://github.com/lydakis/sandfs".into()));
        assert_eq!(parse("https://github.com/lydakis/sandfs.git").0, "lydakis/sandfs");
        assert_eq!(parse("git@github.com:lydakis/sandfs.git").0, "lydakis/sandfs");
        assert!(parse_repo("sandfs", None).is_err());
        assert!(parse_repo("file:///srv/sandfs", None).is_err());
        assert_eq!(parse_repo("file:///srv/sandfs", Some("o/n".into())).unwrap().0, "o/n");
    }
}
