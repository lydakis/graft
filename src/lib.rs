//! Graft (`gft`): work in many fast copies of a GitHub repository, or of a plain folder.
//!
//! Every copy is a sandfs folder over a commit of the repository's default branch, so
//! nothing is cloned and a copy costs what it changes. GitHub stays the place where
//! history, review and merging live: `gft land` turns a copy's changes into a branch
//! and a pull request through GitHub's API, without a local commit or a push. A copy
//! of a plain folder is the folder as it was when copied, and lands by writing its
//! changes back into the folder.
//!
//! Graft keeps only names and pointers (see `state`); sandfs keeps the files and git
//! inside each copy keeps working as in any checkout.

mod github;
mod land;
mod state;
mod tools;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::state::{Copy, Landing, Project, Snapshot, Store, check_copy_name};
use crate::tools::{changes, copy_changes, ensure_mounted, fork, git, mount, mounted, path_str, sandfs, short};

#[derive(Parser)]
#[command(
    name = "gft",
    version,
    about = "Work in fast copies of a GitHub repository that share its files, built on sandfs"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mount a repository's default branch at DIR (default: ./NAME), downloading
    /// nothing up front. This is the home copy, which other copies start from.
    ///
    /// Given a plain folder instead (a path such as ./notes or /data/survey), the
    /// folder itself is the home copy: copies are the folder as it is when made, and
    /// `land` writes their changes back into it.
    Get {
        /// OWNER/NAME on GitHub, the repository's URL, or a folder.
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
        /// Start from this copy's files, as another attempt at the same work: its pull
        /// request goes against the default branch and carries all of them.
        #[arg(long, value_name = "COPY", conflicts_with = "on")]
        from: Option<String>,
        /// Build on top of this copy's landing, as work that depends on it: its pull
        /// request goes against that copy's branch and shows only its own changes.
        #[arg(long, value_name = "COPY")]
        on: Option<String>,
    },
    /// List the copies: how many files each changed, the commit it sits on, its pull
    /// request, and which other copies changed the same files.
    Ls {
        /// Print JSON: each project, with its copies.
        #[arg(long)]
        json: bool,
    },
    /// Describe the copy the current folder is in, or the project from its home copy.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the folder of a copy.
    Path { name: String },
    /// List the files a copy changed (A, M, D), or, given two copies, how A's files
    /// differ from B's (A: only A has it; D: only B has it).
    Changes {
        a: Option<String>,
        b: Option<String>,
        /// Print only the totals, like `3 added, 5 modified, 1 deleted`.
        #[arg(long, conflicts_with = "json")]
        count: bool,
        /// Print JSON: each file's kind (`A`, `M`, `D`) and path.
        #[arg(long)]
        json: bool,
    },
    /// Show the copy's changes line by line, as `land` would send them: against the
    /// commit it sits on (for a copy of a folder, the folder as it took it). Given two
    /// copies A and B, what changes from A to B.
    Diff { a: Option<String>, b: Option<String> },
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
        /// List the snapshots, newest first, numbered as N counts them, and change nothing.
        #[arg(long, conflicts_with = "steps")]
        list: bool,
        #[arg(long, short)]
        copy: Option<String>,
    },
    /// Move a copy onto the newest commit of the default branch, keeping its changes.
    /// Files changed on both sides are merged line by line; conflicts are marked in them.
    /// A copy of a plain folder moves onto the folder as it is now. Programs working
    /// in the copy carry on.
    Sync { name: Option<String> },
    /// Land a copy as a pull request: a branch graft/NAME on GitHub with one commit of
    /// its changes. Landing again adds a commit to the same pull request. A copy of a
    /// plain folder lands by writing its changes into the folder, unless a file it
    /// changed was also changed there since.
    Land {
        name: Option<String>,
        /// The pull request's title, and the commit's message.
        #[arg(long, short)]
        message: Option<String>,
        /// The pull request's description. On a pull request already open, it replaces
        /// the description there.
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the description from a file, or from standard input for `-`.
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,
        /// Open the pull request as a draft.
        #[arg(long)]
        draft: bool,
        /// Land even though the branch on GitHub has commits this copy didn't land,
        /// whose changes this landing then undoes.
        #[arg(long)]
        force: bool,
        /// Print what landing would do (the branch, the commit's parents, the pull
        /// request and the files), changing nothing on GitHub or here.
        #[arg(long, short = 'n')]
        dry_run: bool,
        /// Open a new pull request, on a branch of its own, leaving the copy's earlier
        /// one as it is: for one closed without being merged.
        #[arg(long)]
        new_pr: bool,
        /// Land files that still have conflict markers from `gft sync`.
        #[arg(long)]
        allow_conflicts: bool,
    },
    /// Delete copies, with their snapshots. Their pull requests stay as they are.
    /// A copy with changes that never landed (no pull request, or files changed since
    /// its last landing) is kept unless `--force` is given.
    Rm {
        #[arg(required = true)]
        names: Vec<String>,
        /// Delete the copies even if their changes never landed.
        #[arg(long, short)]
        force: bool,
        /// Also delete each copy's branch on GitHub, once its pull request is merged
        /// or closed; a branch with commits the copy didn't land stays.
        #[arg(long)]
        delete_branch: bool,
    },
    /// Mount copies again after a restart or an unmount left their folders empty:
    /// the copies named, else every copy of the project and its home copy. Other
    /// commands mount the copy they work on themselves.
    Up { names: Vec<String> },
    /// Remove a whole project: every copy, the home copy's mount and what Graft keeps
    /// of it, so `gft get` can start over. A plain folder or a checkout stays as it is.
    /// Copies with changes that never landed are kept, and the project with them,
    /// unless `--force` is given.
    Drop {
        /// OWNER/NAME, or the home copy's folder name (default: the project here).
        project: Option<String>,
        /// Remove copies even if their changes never landed.
        #[arg(long, short)]
        force: bool,
    },
}

/// An error in Graft's words: sandfs, which Graft drives, speaks of workspaces.
fn plain(said: &str) -> String {
    let said = said.replace("the workspace", "the copy").replace("workspace", "copy");
    match said.contains(" is in use by ") {
        true => format!("{said}; another gft command is working on it, so try again in a moment"),
        false => said,
    }
}

/// A sync that left files with conflict markers, which exits with 2.
#[derive(Debug)]
struct Conflicts(usize);

impl std::fmt::Display for Conflicts {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} conflict markers; settle them, then land", if self.0 == 1 { "1 file has" } else { "files have" })
    }
}

impl std::error::Error for Conflicts {}

/// The text of a description file, or of standard input for `-`.
fn read_body(path: &Path) -> Result<String> {
    if path == Path::new("-") {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
            .context("reading the description from standard input")?;
        return Ok(text);
    }
    std::fs::read_to_string(path).with_context(|| format!("reading the description from {}", path.display()))
}

pub fn main() {
    // sandfs serves mounts from a process of its own, which is this program run again
    // as `gft sandfs ...`.
    sandfs::handle_relaunch(&["sandfs"]);
    if let Err(err) = run(Cli::parse()) {
        eprintln!("gft: {}", plain(&format!("{err:#}")));
        std::process::exit(if err.is::<Conflicts>() { 2 } else { 1 });
    }
}

fn run(cli: Cli) -> Result<()> {
    let store = Store::locate()?;
    match cli.command {
        Cmd::Get { repo, dir, github } => get(&store, &repo, dir, github),
        Cmd::New { names, from, on } => match on {
            Some(on) => new(&store, &names, Some((&on, true))),
            None => new(&store, &names, from.as_deref().map(|from| (from, false))),
        },
        Cmd::Ls { json } => ls(&store, json),
        Cmd::Status { json } => status(&store, json),
        Cmd::Path { name } => {
            let (project, _) = here(&store)?;
            println!("{}", copy(&project, &name)?.dir.display());
            Ok(())
        }
        Cmd::Changes { a, b, count, json } => {
            let (project, current) = here(&store)?;
            let workspace = |name: Option<String>| -> Result<(String, PathBuf)> {
                match name.or(current.clone()) {
                    Some(name) => {
                        let copy = copy(&project, &name)?;
                        mount_copy(copy)?;
                        Ok((copy.workspace.clone(), copy.dir.clone()))
                    }
                    None if project.folder => bail!("name a copy: the folder itself is what copies change"),
                    None if project.checkout => bail!("name a copy; in the checkout itself, `git status` says"),
                    None => Ok((project.workspace.clone(), project.home.clone())),
                }
            };
            let (a, dir) = workspace(a)?;
            let changes = match b {
                Some(b) => {
                    let (b, b_dir) = workspace(Some(b))?;
                    between((&a, &dir), (&b, &b_dir))?
                }
                None => changes(&a, None, &dir)?,
            };
            if json {
                let listed: Vec<serde_json::Value> = changes
                    .iter()
                    .map(|change| serde_json::json!({"kind": change.kind.to_string(), "path": change.path}))
                    .collect();
                println!("{}", serde_json::Value::from(listed));
            } else if count {
                let of = |kind| changes.iter().filter(|change| change.kind == kind).count();
                println!("{} added, {} modified, {} deleted", of('A'), of('M'), of('D'));
            } else {
                for change in changes {
                    println!("{} {}", change.kind, change.path);
                }
            }
            Ok(())
        }
        Cmd::Diff { a, b } => {
            let (project, current) = here(&store)?;
            match (a.or(current), b) {
                (Some(a), None) => {
                    let a = copy(&project, &a)?;
                    mount_copy(a)?;
                    diff(&project, a)
                }
                (Some(a), Some(b)) => {
                    let (a, b) = (copy(&project, &a)?, copy(&project, &b)?);
                    mount_copy(a)?;
                    mount_copy(b)?;
                    diff_copies(a, b)
                }
                (None, _) => bail!("name a copy, or run this inside one"),
            }
        }
        Cmd::Save { note, copy: name } => {
            let (project, current) = here(&store)?;
            let name = which(name, current)?;
            let snapshot = save(&store, &project.repo, &name, &note.join(" "))?;
            println!("saved {name} as snapshot {}", snapshot.workspace);
            Ok(())
        }
        Cmd::Undo { list: true, copy: name, .. } => {
            let (project, current) = here(&store)?;
            snapshots(&project, &which(name, current)?)
        }
        Cmd::Undo { steps, copy: name, .. } => {
            let (project, current) = here(&store)?;
            leave_mounts()?;
            undo(&store, &project.repo, &which(name, current)?, steps)
        }
        Cmd::Sync { name } => {
            let (project, current) = here(&store)?;
            let conflicts = match (name, current) {
                (None, None) if !project.folder && !project.checkout => sync_home(&project)?,
                (name, current) => sync(&store, &project.repo, &which(name, current)?)?,
            };
            match conflicts {
                0 => Ok(()),
                conflicts => Err(Conflicts(conflicts).into()),
            }
        }
        Cmd::Land { name, message, body, body_file, draft, force, dry_run, new_pr, allow_conflicts } => {
            let (project, current) = here(&store)?;
            if project.folder {
                if message.is_some() || body.is_some() || body_file.is_some() || draft || force || dry_run || new_pr {
                    bail!(
                        "a copy of a folder lands into the folder; --message, --body, --draft, --force, \
                         --dry-run and --new-pr are for pull requests"
                    );
                }
                return land_folder(&store, &project, &which(name, current)?, allow_conflicts);
            }
            let body = match body_file {
                Some(path) => Some(read_body(&path)?),
                None => body,
            };
            let options = land::Options {
                message: message.as_deref(),
                body: body.as_deref(),
                draft,
                force,
                dry_run,
                new_pr,
                allow_conflicts,
            };
            land::land(&store, &project.repo, &which(name, current)?, &options)
        }
        Cmd::Rm { names, force, delete_branch } => {
            let (project, _) = here(&store)?;
            leave_mounts()?;
            if !force {
                keep_unlanded(&project, &names, "gft rm --force")?;
            }
            for name in &names {
                if delete_branch && !project.folder {
                    println!("{name}: {}", land::delete_branch(&project.repo, copy(&project, name)?)?);
                }
                rm(&store, &project.repo, name)?;
            }
            Ok(())
        }
        Cmd::Up { names } => {
            let (project, _) = here_or_any(&store)?;
            up(&project, &names)
        }
        Cmd::Drop { project, force } => {
            let project = match project {
                Some(named) => store
                    .projects()?
                    .into_iter()
                    .find(|project| project.repo == named || project.workspace == named || home_name(project) == named)
                    .with_context(|| format!("no project {named}; `gft ls` lists them"))?,
                None => here(&store)?.0,
            };
            leave_mounts()?;
            drop_project(&store, &project, force)
        }
    }
}

/// Refuse when any of the copies `names` has changes that never landed, all checked
/// before any is deleted, so a refusal leaves every copy as it was.
fn keep_unlanded(project: &Project, names: &[String], anyway: &str) -> Result<()> {
    let mut kept = Vec::new();
    for name in names {
        let lost = unlanded(copy(project, name)?)?;
        if lost > 0 {
            kept.push(format!("{name} has {} that never landed", files(lost)));
        }
    }
    if !kept.is_empty() {
        bail!("{}; `gft land` them first, or `{anyway}` to delete them anyway", kept.join(", "));
    }
    Ok(())
}

/// Remove every copy of `project`, then the project itself.
fn drop_project(store: &Store, project: &Project, force: bool) -> Result<()> {
    let names: Vec<String> = project.copies.keys().cloned().collect();
    if !force {
        keep_unlanded(project, &names, "gft drop --force")?;
        // Files changed in the home copy of a repository go with its mount.
        if !project.folder && !project.checkout && mounted(&project.workspace)?.is_some() {
            let changed = changes(&project.workspace, None, &project.home)?.len();
            if changed > 0 {
                bail!(
                    "{} has {} changed in it, which would be lost; copy them out first, or `gft drop --force`",
                    project.home.display(),
                    files(changed)
                );
            }
        }
    }
    for name in &names {
        rm(store, &project.repo, name)?;
    }
    // The home copy of a repository is a mount Graft made; a folder or a checkout is
    // the user's own.
    if !project.folder && !project.checkout {
        let home = sandfs()?;
        for workspace in [project.workspace.clone(), main_workspace(project)] {
            let known = home.workspaces()?.into_iter().find(|known| known.name == workspace);
            if let Some(known) = known {
                let removed = home.remove(&workspace, known.mounted.is_some());
                in_use(&project.home, removed)?;
            }
        }
        // Empty once unmounted.
        let _ = std::fs::remove_dir(&project.home);
    }
    store.forget(&project.repo)?;
    println!("dropped {} ({})", project.repo, project.home.display());
    Ok(())
}

/// The project the current folder is in, and the copy, which is mounted if it
/// isn't, as after a restart.
fn here(store: &Store) -> Result<(Project, Option<String>)> {
    let (project, current) = here_or_any(store)?;
    if let Some(name) = &current {
        let copy = copy(&project, name)?;
        if mounted(&copy.workspace)?.is_none() {
            mount_copy(copy)?;
            eprintln!(
                "gft: mounted {name} again; a shell already in its folder needs `cd {}` to see it",
                copy.dir.display()
            );
        }
    }
    Ok((project, current))
}

/// The project the current folder is in, and the copy, as they are.
fn here_or_any(store: &Store) -> Result<(Project, Option<String>)> {
    store.find(&std::env::current_dir()?)
}

/// Mount the copies `names` of `project`, or all of them and its home copy.
fn up(project: &Project, names: &[String]) -> Result<()> {
    let mut wanted: Vec<(String, String, PathBuf)> = Vec::new();
    if names.is_empty() {
        if !project.folder && !project.checkout {
            wanted.push((project.workspace.clone(), project.workspace.clone(), project.home.clone()));
        }
        for (name, copy) in &project.copies {
            wanted.push((name.clone(), copy.workspace.clone(), copy.dir.clone()));
        }
    }
    for name in names {
        let copy = copy(project, name)?;
        wanted.push((name.clone(), copy.workspace.clone(), copy.dir.clone()));
    }
    let mounted: BTreeSet<String> =
        sandfs()?.workspaces()?.into_iter().filter(|ws| ws.mounted.is_some()).map(|ws| ws.name).collect();
    let mut brought = 0;
    for (name, workspace, dir) in wanted {
        if mounted.contains(&workspace) {
            continue;
        }
        ensure_mounted(&workspace, &dir)?;
        println!("mounted {name} at {}", dir.display());
        brought += 1;
    }
    if brought == 0 {
        println!("all mounted");
    }
    Ok(())
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
    project.copies.get(name).with_context(|| {
        let names: Vec<&str> = project.copies.keys().map(String::as_str).collect();
        if names.is_empty() {
            format!("{} has no copy {name:?} (it has no copies yet); make one with `gft new {name}`", project.repo)
        } else {
            format!("{} has no copy {name:?}; its copies are: {}", project.repo, names.join(", "))
        }
    })
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// A new base for a copy of a folder, `N@SECONDS`: the project's next take of the
/// folder, and when it was taken. `undo` tells bases apart by N alone, since two
/// takes can share any clock reading.
fn folder_base(project: &mut Project) -> String {
    project.takes += 1;
    format!("{}@{}", project.takes, now())
}

/// When a copy of a folder took its base, in seconds.
fn copied_at(base: &str) -> Option<u64> {
    base.split_once('@')?.1.parse().ok()
}

/// The home copy's folder name, which names it on the command line.
fn home_name(project: &Project) -> String {
    project.home.file_name().map_or_else(|| project.workspace.clone(), |name| name.to_string_lossy().into_owned())
}

/// How many seconds the default branch counts as up to date once fetched.
const FRESH: u64 = 10;

/// The workspace that holds the default branch as last fetched, which copies fork.
fn main_workspace(project: &Project) -> String {
    format!("{}._main", project.workspace)
}

/// `owner/name` from a path such as `owner/name`, `owner/name.git` or `owner/name/`.
/// Anything else (a lone name, extra folders, odd characters) is not a repository.
fn owner_name(path: &str) -> Option<String> {
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    let valid = |part: &str| {
        !matches!(part, "" | "." | "..") && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (valid(owner) && valid(name)).then(|| format!("{owner}/{name}"))
}

/// The host and path of a URL (`https://host/path`, `ssh://git@host/path`) or of the
/// `git@host:path` form that git also accepts.
fn host_and_path(repo: &str) -> Option<(&str, &str)> {
    let (authority, path) = match repo.split_once("://") {
        Some((_, rest)) => rest.split_once('/').unwrap_or((rest, "")),
        None => repo.split_once(':')?,
    };
    let host = authority.rsplit('@').next()?.split(':').next()?;
    Some((host, path.split(['?', '#']).next()?))
}

/// `owner/name` and the URL to mount for what `gft get` was given.
fn parse_repo(repo: &str, github: Option<String>) -> Result<(String, String)> {
    let github = github.map(|text| owner_name(&text).context("--github takes OWNER/NAME")).transpose()?;
    if repo.contains("://") || repo.contains('@') {
        // The host decides, so a github.com that only appears in the path doesn't count.
        let (host, path) = host_and_path(repo).with_context(|| format!("can't read {repo} as a repository URL"))?;
        let on_github = host.eq_ignore_ascii_case("github.com") || host.eq_ignore_ascii_case("www.github.com");
        let slug = match github {
            Some(slug) => slug,
            None if on_github => {
                owner_name(path.trim_start_matches('/')).context("can't tell the repository's owner and name")?
            }
            None => bail!("{repo} isn't on github.com; say where copies land with --github OWNER/NAME"),
        };
        return Ok((slug, repo.to_owned()));
    }
    let slug = owner_name(repo).context("give the repository as OWNER/NAME")?;
    let url = format!("https://github.com/{slug}");
    Ok((github.unwrap_or(slug), url))
}

/// `repo` as a folder on this machine, when it is a path (one starting with `/`, `.` or
/// `~`, or a lone name) to a folder. `OWNER/NAME` always names a repository.
fn as_folder(repo: &str) -> Option<PathBuf> {
    let path = match repo.strip_prefix("~/") {
        Some(rest) => std::env::home_dir()?.join(rest),
        None => PathBuf::from(repo),
    };
    let path_like = repo.starts_with(['/', '.', '~']) || !repo.contains(['/', ':', '@']);
    (path_like && path.is_dir()).then_some(path)
}

fn get(store: &Store, repo: &str, dir: Option<PathBuf>, github: Option<String>) -> Result<()> {
    if let Some(folder) = as_folder(repo) {
        if dir.is_some() {
            bail!("a folder is its own home copy; DIR is for repositories on GitHub");
        }
        if folder.join(".git").exists() {
            return get_checkout(store, &folder, github);
        }
        if github.is_some() {
            bail!("a plain folder lands into itself; --github is for repositories");
        }
        return get_folder(store, &folder);
    }
    if repo.starts_with(['/', '.', '~']) {
        bail!("{repo} is no folder");
    }
    let (slug, source) = parse_repo(repo, github)?;
    let short_name = slug.rsplit('/').next().expect("owner/name").to_owned();
    let dir = std::path::absolute(dir.unwrap_or_else(|| PathBuf::from(&short_name)))?;
    already(store, &slug)?;
    let name = free_workspace(store, &slug)?;
    mount(&source, &dir, &name)?;
    let dir = dir.canonicalize()?;
    let mut project = Project {
        repo: slug.clone(),
        source,
        home: dir.clone(),
        workspace: name.clone(),
        branch: String::new(),
        folder: false,
        checkout: false,
        takes: 0,
        fetched: now(),
        copies: BTreeMap::new(),
    };
    // Kept before anything changes the home copy, as the start of every new copy.
    fork(&name, None, &main_workspace(&project))?;
    project.branch = git(&dir, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_else(|_| "main".to_owned());
    let commit = git(&dir, &["rev-parse", "HEAD"])?;
    store.create(&project)?;
    println!("{}: {slug} at {} ({})", dir.display(), short(&commit), project.branch);
    Ok(())
}

/// Refuse to make `repo` a project again, saying where it is.
fn already(store: &Store, repo: &str) -> Result<()> {
    if store.exists(repo) {
        let home = store.load(repo)?.home;
        bail!("{repo} is a project already, at {}; `gft drop {repo}` removes it to start over", home.display());
    }
    Ok(())
}

/// A name for the workspaces of the repository `repo` (`OWNER/NAME`): NAME, unless
/// another project or a workspace of sandfs's has it, as when two owners' forks share
/// a name; then OWNER-NAME, then OWNER-NAME-2 and on.
fn free_workspace(store: &Store, repo: &str) -> Result<String> {
    let (owner, name) = repo.split_once('/').context("OWNER/NAME")?;
    let projects: HashSet<String> = store.projects()?.into_iter().map(|project| project.workspace).collect();
    let workspaces: HashSet<String> = sandfs()?.workspaces()?.into_iter().map(|workspace| workspace.name).collect();
    let free = |candidate: &String| {
        !projects.contains(candidate)
            && !workspaces.contains(candidate)
            && !workspaces.contains(&format!("{candidate}._main"))
    };
    let tidy = |text: &str| -> String {
        text.chars().map(|c| if c.is_ascii_alphanumeric() || "_-".contains(c) { c } else { '-' }).collect()
    };
    let qualified = tidy(&format!("{owner}-{name}"));
    std::iter::once(tidy(name))
        .chain(std::iter::once(qualified.clone()))
        .chain((2..).map(|n| format!("{qualified}-{n}")))
        .find(free)
        .context("no free name")
}

/// Make the git checkout `folder` a project, whose home copy is the checkout itself:
/// copies start from its commit and its uncommitted changes, and land on GitHub.
fn get_checkout(store: &Store, folder: &Path, github: Option<String>) -> Result<()> {
    let folder = folder.canonicalize()?;
    let slug = match (github, git(&folder, &["remote", "get-url", "origin"]).ok()) {
        (Some(github), _) => owner_name(&github).context("--github takes OWNER/NAME")?,
        (None, Some(url)) => parse_repo(&url, None).map(|(slug, _)| slug).with_context(|| {
            format!(
                "{}'s origin, {url}, isn't on GitHub; say where copies land with --github OWNER/NAME",
                folder.display()
            )
        })?,
        (None, None) => {
            bail!("{} has no remote named origin; say where copies land with --github OWNER/NAME", folder.display())
        }
    };
    already(store, &slug)?;
    let name = free_workspace(store, &slug)?;
    // Pull requests go against the default branch: as the checkout last heard of it
    // from origin, else as GitHub says.
    let known = git(&folder, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .ok()
        .and_then(|branch| branch.strip_prefix("origin/").map(str::to_owned));
    let branch = match known {
        Some(branch) => branch,
        None => github::GitHub::new(&slug)
            .and_then(|github| github.default_branch())
            .with_context(|| format!("asking GitHub for {slug}'s default branch, which pull requests go against"))?,
    };
    let project = Project {
        repo: slug.clone(),
        source: path_str(&folder)?.to_owned(),
        home: folder.clone(),
        workspace: name,
        branch,
        folder: false,
        checkout: true,
        takes: 0,
        fetched: 0,
        copies: BTreeMap::new(),
    };
    store.create(&project)?;
    println!(
        "{}: {slug} from this checkout; `gft new NAME` copies its commit and uncommitted changes next to it",
        folder.display()
    );
    Ok(())
}

/// Make the plain folder `folder` a project, whose home copy is the folder itself.
fn get_folder(store: &Store, folder: &Path) -> Result<()> {
    let folder = folder.canonicalize()?;
    if folder.join(".git").exists() {
        bail!(
            "{} is a git repository; `gft get OWNER/NAME` works on its repository on GitHub instead",
            folder.display()
        );
    }
    let name = folder.file_name().context("the folder has no name")?.to_string_lossy().into_owned();
    check_copy_name(&name).with_context(|| format!("{} can't name a project", folder.display()))?;
    if let Some(other) = store.projects()?.into_iter().find(|project| project.workspace == name) {
        bail!("{} is already a project named {name}; Graft names a project after its folder", other.home.display());
    }
    let project = Project {
        repo: format!("folder/{name}"),
        source: path_str(&folder)?.to_owned(),
        home: folder.clone(),
        workspace: name,
        branch: String::new(),
        folder: true,
        checkout: false,
        takes: 0,
        fetched: 0,
        copies: BTreeMap::new(),
    };
    store.create(&project)?;
    println!("{}: a plain folder; `gft new NAME` makes a copy of it next to it", folder.display());
    Ok(())
}

/// Make copies of the default branch, or of the copy `from`: built on top of its
/// landing when `on` is set, else as another attempt.
fn new(store: &Store, names: &[String], from: Option<(&str, bool)>) -> Result<()> {
    let (project, _) = here(store)?;
    for (i, name) in names.iter().enumerate() {
        check_copy_name(name)?;
        if names[..i].contains(name) {
            bail!("{name:?} is given twice; each new copy needs its own name");
        }
        if project.copies.contains_key(name) {
            bail!("{} already has a copy {name:?}; pick another name, or remove it with `gft rm {name}`", project.repo);
        }
    }
    // A copy built on top of another sits on that one's landing, as it is now.
    let mut stacked = None;
    // Held while the default branch is brought up to date and copied, which
    // `gft new` run at once in many places (an agent each) take turns at.
    let mut _main = None;
    let (source, mut base) = match from {
        // The home copy, with what was changed there.
        Some((name, on))
            if !project.copies.contains_key(name) && (name == project.workspace || name == home_name(&project)) =>
        {
            if on {
                bail!("the home copy never lands, so there is nothing to build on; use --from {name}");
            }
            match project.folder || project.checkout {
                true => (project.source.clone(), None),
                false => {
                    ensure_mounted(&project.workspace, &project.home)?;
                    (project.workspace.clone(), Some(git(&project.home, &["rev-parse", "HEAD"])?))
                }
            }
        }
        Some((name, on)) => {
            let from = copy(&project, name)?;
            if on {
                if project.folder {
                    bail!("a copy of a folder lands into the folder, so there is nothing to build on; use --from");
                }
                let Some(landed) = &from.landed else {
                    bail!(
                        "{name} hasn't landed, so there is no branch to build on: land it first (a draft will do), \
                         or start from its files with --from"
                    );
                };
                // A landing kept by Graft before 0.1 doesn't say its commit until that
                // copy lands again.
                if landed.commit.is_empty() {
                    bail!("land {name} once more first: Graft before 0.1 didn't keep which commit it landed");
                }
                stacked = Some(Landing { files: BTreeMap::new(), ..landed.clone() });
            }
            (from.workspace.clone(), Some(from.base.clone()))
        }
        // Each copy is the folder, or the checkout, as it is when made.
        None if project.folder || project.checkout => (project.source.clone(), None),
        None => {
            // Brings the default branch up to date once for all the copies made now,
            // unless that was done moments ago.
            _main = Some(store.lock_main(&project.repo)?);
            let main = main_workspace(&project);
            if now().saturating_sub(store.load(&project.repo)?.fetched) >= FRESH {
                sandfs()?.update(&main, None)?;
                store.edit(&project.repo, |project| {
                    project.fetched = now();
                    Ok(())
                })?;
            }
            (main, None)
        }
    };
    let parent = project.home.parent().context("the home copy has no parent folder")?;
    let home_name = project.home.file_name().context("the home copy has no folder name")?.to_string_lossy();
    for name in names {
        let dir = parent.join(format!("{home_name}+{name}"));
        let workspace = format!("{}.{name}", project.workspace);
        fork(&source, Some(&dir), &workspace)?;
        if base.is_none() && !project.folder {
            base = Some(git(&dir, &["rev-parse", "HEAD"])?);
        }
        let dir = dir.canonicalize()?;
        store.edit(&project.repo, |project| {
            let base = match &base {
                Some(commit) => commit.clone(),
                None => folder_base(project),
            };
            let copy = Copy {
                dir: dir.clone(),
                workspace,
                base,
                from: from.map(|(from, _)| from.to_owned()),
                stacked: stacked.clone(),
                landed: None,
                pending: None,
                pr: None,
                conflicts: BTreeSet::new(),
                snapshots: Vec::new(),
                next: 1,
            };
            project.copies.insert(name.clone(), copy);
            Ok(())
        })?;
        println!("{}", dir.display());
    }
    Ok(())
}

/// The project of the current copy; outside any copy, every project Graft knows.
fn ls(store: &Store, json: bool) -> Result<()> {
    let projects = match here(store) {
        Ok((project, _)) => vec![project],
        Err(err) => {
            let mut projects = store.projects()?;
            if projects.is_empty() {
                return Err(err);
            }
            projects.sort_by(|a, b| a.repo.cmp(&b.repo));
            projects
        }
    };
    let listed = projects.iter().map(|project| list(project, !json)).collect::<Result<Vec<_>>>()?;
    if json {
        println!("{}", serde_json::Value::from(listed));
    }
    Ok(())
}

/// The project and its copies: printed when `print`, and as JSON.
fn list(project: &Project, print: bool) -> Result<serde_json::Value> {
    let kind = match (project.folder, project.checkout) {
        (true, _) => "folder".to_owned(),
        (_, true) => format!("checkout, {}", project.branch),
        _ => format!("home, {}", project.branch),
    };
    if print {
        println!("{}\t{}\t({kind})", project.workspace, project.home.display());
    }
    let mut listed = Vec::new();
    let mounted: BTreeSet<String> =
        sandfs()?.workspaces()?.into_iter().filter(|ws| ws.mounted.is_some()).map(|ws| ws.name).collect();
    // Each copy's changes are read at once, a few copies at a time.
    let copies: Vec<(&String, &Copy)> = project.copies.iter().collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let mut changed: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    let found: Vec<Result<BTreeSet<String>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = copies
            .chunks(copies.len().div_ceil(threads).max(1))
            .map(|chunk| {
                scope.spawn(move || {
                    let paths = |copy: &Copy| {
                        changes(&copy.workspace, None, &copy.dir)
                            .map(|changes| changes.into_iter().map(|change| change.path).collect())
                    };
                    chunk.iter().map(|(_, copy)| paths(copy)).collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    for ((name, _), paths) in copies.iter().zip(found) {
        changed.insert(name, paths?);
    }
    for (name, copy) in &project.copies {
        let mine = &changed[name.as_str()];
        let mut notes = Vec::new();
        let mut overlaps = serde_json::Map::new();
        let up = mounted.contains(&copy.workspace);
        if !up {
            notes.push("not mounted: `gft up` brings it back".to_owned());
        }
        for (other, theirs) in &changed {
            if other == name {
                continue;
            }
            // The same bytes in both, as a copy made from another has until it
            // changes them, is no clash. Only mounted copies show their bytes.
            let other_copy = &project.copies[*other];
            let both_up = up && mounted.contains(&other_copy.workspace);
            let shared: Vec<&String> = mine
                .intersection(theirs)
                .filter(|path| !(both_up && same_file(&copy.dir, &other_copy.dir, path)))
                .collect();
            if let Some(first) = shared.first() {
                let more = if shared.len() > 1 { format!(" and {} more", shared.len() - 1) } else { String::new() };
                notes.push(format!("overlaps {other} on {first}{more}"));
                overlaps.insert(other.to_string(), serde_json::json!(shared));
            }
        }
        if print {
            let pr = copy.pr.as_ref().map_or("-".to_owned(), |pr| pr.url.clone());
            let files = files(mine.len());
            println!("{name}\t{files}\t{}\t{pr}\t{}", based(project, &copy.base), notes.join("; "));
        }
        listed.push(serde_json::json!({
            "name": name, "folder": copy.dir, "changed": mine.len(), "base": copy.base,
            "pull_request": copy.pr.as_ref().map(|pr| &pr.url), "mounted": up, "overlaps": overlaps,
        }));
    }
    Ok(serde_json::json!({
        "project": project.repo, "home": project.home, "kind": kind.split(',').next(),
        "branch": project.branch, "copies": listed,
    }))
}

/// Whether `path` is the same in the folders `a` and `b`: the same bytes and the same
/// executable bit, as git sees them, a link to the same place, or no file in either.
fn same_file(a: &Path, b: &Path, path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let read = |dir: &Path| -> Option<(char, Vec<u8>)> {
        let full = dir.join(path);
        let meta = entry(dir, path)?;
        if meta.file_type().is_symlink() {
            return Some(('l', std::fs::read_link(&full).ok()?.into_os_string().into_encoded_bytes()));
        }
        let kind = if meta.permissions().mode() & 0o111 != 0 { 'x' } else { 'f' };
        meta.is_file().then(|| std::fs::read(&full).ok().map(|data| (kind, data))).flatten()
    };
    read(a) == read(b)
}

/// What a copy sits on: `base COMMIT`, or for a copy of a folder `copied … ago`.
fn based(project: &Project, base: &str) -> String {
    match copied_at(base) {
        Some(time) if project.folder => format!("copied {}", ago(now().saturating_sub(time))),
        _ => format!("base {}", short(base)),
    }
}

fn files(n: usize) -> String {
    match n {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    }
}

/// How many of the copy's files would be lost: every changed one when it never
/// landed, else each not as the last landing left it. That counts a file the landing
/// carried that the copy put back as it was before the landing, a change of its own;
/// not one a newer base has some other way, as once its pull request is merged,
/// upstream changes the file again, and the copy syncs.
fn unlanded(copy: &Copy) -> Result<usize> {
    mount_copy(copy)?;
    let changed = changes(&copy.workspace, None, &copy.dir)?;
    let Some(landed) = &copy.landed else { return Ok(changed.len()) };
    let changed: BTreeSet<&String> = changed.iter().map(|change| &change.path).collect();
    let mut lost = 0;
    for path in changed.iter().copied().chain(landed.files.keys().filter(|path| !changed.contains(path))) {
        let now = land::fingerprint(&copy.dir, path)?;
        if landed.files.get(path) == Some(&now) {
            continue;
        }
        if changed.contains(path) || now == land::fingerprint_at(&copy.dir, &landed.on, path)? {
            lost += 1;
        }
    }
    Ok(lost)
}

fn status(store: &Store, json: bool) -> Result<()> {
    let (project, current) = here(store)?;
    let Some(name) = current else {
        let mut about = serde_json::json!({"project": project.repo, "folder": project.home,
            "copies": project.copies.len()});
        if !json {
            println!("project  {}", project.repo);
            println!("folder   {}", project.home.display());
            println!("copies   {}", project.copies.len());
        }
        if !project.folder && !project.checkout {
            // The home copy is where copies come from, not one that lands: say what
            // it holds that they don't.
            let commit = git(&project.home, &["rev-parse", "HEAD"])?;
            let changed = changes(&project.workspace, None, &project.home)?.len();
            about["base"] = commit.clone().into();
            about["changed"] = changed.into();
            if !json {
                println!("on commit  {}", short(&commit));
                if changed > 0 {
                    println!(
                        "changed    {} here, which new copies don't have and nothing lands; `gft new NAME --from {}` \
                         copies them",
                        files(changed),
                        home_name(&project)
                    );
                }
            }
        }
        if json {
            println!("{about}");
        }
        return Ok(());
    };
    let copy = copy(&project, &name)?;
    let changed = changes(&copy.workspace, None, &copy.dir)?.len();
    let marked = marked(copy, false)?;
    if json {
        let about = serde_json::json!({"copy": name, "folder": copy.dir, "base": copy.base, "changed": changed,
            "conflicts": marked, "snapshots": copy.snapshots.len(),
            "pull_request": copy.pr.as_ref().map(|pr| &pr.url)});
        println!("{about}");
        return Ok(());
    }
    println!("copy       {name}");
    println!("folder     {}", copy.dir.display());
    match project.folder {
        true => println!("copied     {}", ago(now().saturating_sub(copied_at(&copy.base).unwrap_or(0)))),
        false => println!("on commit  {}", short(&copy.base)),
    }
    println!("changed    {}", files(changed));
    if !marked.is_empty() {
        let paths: Vec<&str> = marked.iter().map(String::as_str).collect();
        println!("conflicts  {} (settle them before landing)", paths.join(", "));
    }
    println!("snapshots  {}", copy.snapshots.len());
    if let Some(pr) = &copy.pr {
        println!("pull request  {}", pr.url);
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
    fork(&workspace, None, &snapshot_name)?;
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        let snapshot =
            Snapshot { workspace: snapshot_name, base: copy.base.clone(), note: note.to_owned(), time: now() };
        copy.snapshots.push(snapshot.clone());
        Ok(snapshot)
    })
}

/// Print the copy's snapshots, newest first, numbered as `undo N` counts them.
fn snapshots(project: &Project, name: &str) -> Result<()> {
    let copy = copy(project, name)?;
    if copy.snapshots.is_empty() {
        println!("{name} has no snapshots; `gft save` keeps one");
    }
    let now = now();
    for (n, snapshot) in copy.snapshots.iter().rev().enumerate() {
        let note = if snapshot.note.is_empty() { "(no note)" } else { &snapshot.note };
        println!("{}\t{}\t{note}", n + 1, ago(now.saturating_sub(snapshot.time)));
    }
    Ok(())
}

/// How long ago, for a number of seconds, in the largest unit that fits.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} sec ago"),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} hr ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

/// Put the copy back as its snapshot `steps` back has it. What it is now is saved
/// first, unless a snapshot already has it, and newer snapshots stay: undoing again
/// goes back to how it was.
fn undo(store: &Store, repo: &str, name: &str, steps: usize) -> Result<()> {
    let project = store.load(repo)?;
    let copy = copy(&project, name)?.clone();
    if steps == 0 || steps > copy.snapshots.len() {
        bail!("{name} has {} snapshots; `gft save` keeps one", copy.snapshots.len());
    }
    let target = copy.snapshots[copy.snapshots.len() - steps].clone();
    mount_copy(&copy)?;
    let kept = save_unless_saved(store, repo, name, "before undo")?;
    if target.base == copy.base {
        // Written back through the mount, file by file, so programs working in the
        // copy (a shell sitting in it) carry on undisturbed. Git's own files stay.
        copy_changes(&target.workspace, Some(&copy.workspace), &copy.dir)?;
    } else {
        // A snapshot from before a sync sits on another commit: the copy is made again.
        in_use(&copy.dir, sandfs()?.remove(&copy.workspace, true))?;
        fork(&target.workspace, Some(&copy.dir), &copy.workspace)?;
    }
    // A snapshot taken before conflicts were settled brings their markers back.
    let marked = marked(&copy, true)?;
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.base = target.base.clone();
        copy.conflicts = marked.clone();
        Ok(())
    })?;
    let note = if target.note.is_empty() { String::new() } else { format!(" ({})", target.note) };
    let back = match kept {
        Some(_) => "; `gft undo` again returns to how it was",
        None => "",
    };
    println!("{name} is back at snapshot {}{note}{back}", target.workspace);
    Ok(())
}

/// Save the copy as it is now, unless its newest snapshot has it as it is.
fn save_unless_saved(store: &Store, repo: &str, name: &str, note: &str) -> Result<Option<Snapshot>> {
    let project = store.load(repo)?;
    let copy = copy(&project, name)?;
    if let Some(last) = copy.snapshots.last()
        && last.base == copy.base
        && tools::same(&copy.workspace, &last.workspace)?
    {
        return Ok(None);
    }
    save(store, repo, name, note).map(Some)
}

/// Delete the snapshot `snapshot` of the copy `name`.
fn drop_snapshot(store: &Store, repo: &str, name: &str, snapshot: &Snapshot) -> Result<()> {
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.snapshots.retain(|kept| kept.workspace != snapshot.workspace);
        Ok(())
    })?;
    sandfs()?.remove(&snapshot.workspace, false)
}

/// Print the copy's changes against what it started from, as a unified diff.
fn diff(project: &Project, copy: &Copy) -> Result<()> {
    let changed = changes(&copy.workspace, None, &copy.dir)?;
    if project.folder {
        let taken = taken_folder(copy)?;
        for change in &changed {
            let old = taken.join(&change.path);
            // A link counts as there even when what it points at isn't.
            let there = entry(&taken, &change.path).is_some();
            let old = if there { path_str(&old)?.to_owned() } else { "/dev/null".to_owned() };
            let new = if change.kind == 'D' { "/dev/null".to_owned() } else { change.path.clone() };
            show_diff(&copy.dir, &["--no-index", "--", &old, &new])?;
        }
        return Ok(());
    }
    // Files the base has, in one go; added ones against nothing, one by one.
    let known: Vec<&str> = changed.iter().filter(|c| c.kind != 'A').map(|c| c.path.as_str()).collect();
    for chunk in known.chunks(500) {
        let mut args = vec![copy.base.as_str(), "--"];
        args.extend(chunk);
        show_diff(&copy.dir, &args)?;
    }
    for change in changed.iter().filter(|change| change.kind == 'A') {
        show_diff(&copy.dir, &["--no-index", "--", "/dev/null", &change.path])?;
    }
    Ok(())
}

/// Print what changes from copy `a` to copy `b`, as a unified diff.
fn diff_copies(a: &Copy, b: &Copy) -> Result<()> {
    let parent = b.dir.parent().context("a copy has no parent folder")?;
    let name = |copy: &Copy| copy.dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let (a_name, b_name) = (name(a), name(b));
    for change in between((&b.workspace, &b.dir), (&a.workspace, &a.dir))? {
        let old = if change.kind == 'A' { "/dev/null".to_owned() } else { format!("{a_name}/{}", change.path) };
        let new = if change.kind == 'D' { "/dev/null".to_owned() } else { format!("{b_name}/{}", change.path) };
        show_diff(parent, &["--no-index", "--", &old, &new])?;
    }
    Ok(())
}

/// How the files of the copy `a` (its workspace and folder) differ from those of `b`:
/// `A` for a file only `a` has, `D` for one only `b` has, `M` for one both have.
fn between(a: (&str, &Path), b: (&str, &Path)) -> Result<Vec<tools::Change>> {
    // sandfs compares copies on the same commit; others are compared here, file by
    // file, over what either changed and what upstream changed between their commits.
    if let Ok(found) = changes(a.0, Some(b.0), a.1) {
        return Ok(found);
    }
    let mut paths = BTreeSet::new();
    for (workspace, dir) in [a, b] {
        paths.extend(changes(workspace, None, dir)?.into_iter().map(|change| change.path));
    }
    if let (Ok(from), Ok(to)) = (git(a.1, &["rev-parse", "HEAD"]), git(b.1, &["rev-parse", "HEAD"]))
        && from != to
    {
        let listed = tools::git_raw(a.1, &["diff", "--name-only", "-z", &from, &to])?;
        let listed = listed.split(|&byte| byte == 0).filter(|path| !path.is_empty());
        paths.extend(listed.map(|path| String::from_utf8_lossy(path).into_owned()));
    }
    let is_file = |dir: &Path, path: &str| entry(dir, path).is_some_and(|meta| !meta.is_dir());
    Ok(paths
        .into_iter()
        .filter(|path| !same_file(a.1, b.1, path))
        .filter_map(|path| {
            let kind = match (is_file(a.1, &path), is_file(b.1, &path)) {
                (true, true) => 'M',
                (true, false) => 'A',
                (false, true) => 'D',
                (false, false) => return None,
            };
            Some(tools::Change { kind, path })
        })
        .collect())
}

/// Run `git diff` in `dir` with `args`, onto standard output.
fn show_diff(dir: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=off", "diff", "--no-color", "--no-ext-diff"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .status()
        .context("running git diff")?;
    // 1 is "they differ" with --no-index.
    match status.code() {
        Some(0 | 1) => Ok(()),
        _ => bail!("git diff failed"),
    }
}

/// The folder as a copy of a plain folder took it, which sandfs keeps until the copy
/// moves on.
fn taken_folder(copy: &Copy) -> Result<PathBuf> {
    let record = sandfs()?.root().join("workspaces").join(&copy.workspace).join("workspace.json");
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&record)?)?;
    record["base"]["path"].as_str().map(PathBuf::from).context("can't tell what folder the copy started from")
}

/// Refuse to land files that still have conflict markers, `marked`, unless `allow`.
fn refuse_conflicts(marked: &BTreeSet<String>, allow: bool) -> Result<()> {
    if marked.is_empty() || allow {
        return Ok(());
    }
    let paths: Vec<&str> = marked.iter().map(String::as_str).collect();
    bail!(
        "{} still {} conflict markers from `gft sync`: {}. Settle them first, or land with --allow-conflicts \
         to publish them as they are",
        files(marked.len()),
        if marked.len() == 1 { "has" } else { "have" },
        paths.join(", ")
    )
}

/// The files `gft sync` left with conflict markers that still have them; with
/// `everywhere`, any file the copy changed that has Graft's markers, as one put back
/// by `undo` may.
fn marked(copy: &Copy, everywhere: bool) -> Result<BTreeSet<String>> {
    let mut paths = copy.conflicts.clone();
    if everywhere {
        paths.extend(changes(&copy.workspace, None, &copy.dir)?.into_iter().map(|change| change.path));
    }
    Ok(markers_in(&copy.dir, &paths))
}

/// Whether something other than a folder, such as a link, stands above `path` in
/// the folder `dir`, so that `path` there would reach out of it.
fn under_link(dir: &Path, path: &str) -> bool {
    let mut at = dir.to_path_buf();
    let parents = Path::new(path).parent().into_iter().flat_map(Path::components);
    parents.into_iter().any(|part| {
        at.push(part);
        std::fs::symlink_metadata(&at).is_ok_and(|meta| !meta.is_dir())
    })
}

/// What is at `path` in the folder `dir`, without following a link there or above
/// it: none for nothing, or for a path under a link.
fn entry(dir: &Path, path: &str) -> Option<std::fs::Metadata> {
    if under_link(dir, path) {
        return None;
    }
    std::fs::symlink_metadata(dir.join(path)).ok()
}

/// Whether `path` in the folder `dir` is a regular file, not a link to one nor under one.
fn regular(dir: &Path, path: &str) -> bool {
    entry(dir, path).is_some_and(|meta| meta.is_file())
}

/// Which of `paths` in the folder `dir` have Graft's conflict markers.
fn markers_in(dir: &Path, paths: &BTreeSet<String>) -> BTreeSet<String> {
    let mut marked = BTreeSet::new();
    for path in paths {
        // Only a file's own bytes land: a link lands as where it points, unread.
        if !regular(dir, path) {
            continue;
        }
        let Ok(data) = std::fs::read(dir.join(path)) else { continue };
        let lines = data.split(|&byte| byte == b'\n');
        if lines.into_iter().any(|line| line.starts_with(b"<<<<<<< yours") || line.starts_with(b">>>>>>> upstream")) {
            marked.insert(path.clone());
        }
    }
    marked
}

/// An unmount of the folder `dir` refused because something works in it, said
/// plainly: Linux says busy, macOS suggests its own unmount command.
fn in_use<T>(dir: &Path, result: Result<T>) -> Result<T> {
    result.map_err(|err| {
        let said = format!("{err:#}");
        if said.contains("busy") || said.contains("diskutil") {
            anyhow::anyhow!(
                "{} is in use: a shell or program is working in it (its folder or a file in it is open); \
                 leave it, as with `cd ..`, and try again",
                dir.display()
            )
        } else {
            err
        }
    })
}

/// Work of a copy's that GitHub merged since the copy last synced: its own last
/// landing, or the landing it was built on. The copy's files started from that
/// landing, so it, not the copy's base, is what upstream's version is merged from.
struct Merged {
    github: github::GitHub,
    /// The landing's commit.
    commit: String,
    /// What happened, for people.
    said: String,
    /// The pull request of the copy's own that is over: closed with the branch under
    /// it, or merged.
    done: bool,
}

/// What of the copy's GitHub merged since it last synced, if anything.
fn merged(project: &Project, copy: &Copy, name: &str) -> Result<Option<Merged>> {
    if project.folder || (copy.stacked.is_none() && copy.pr.is_none()) {
        return Ok(None);
    }
    let github = github::GitHub::new(&project.repo)?;
    let pull = match &copy.pr {
        Some(pr) => Some((pr.number, github.call("GET", &format!("pulls/{}", pr.number), None)?)),
        None => None,
    };
    // Merged into the default branch, that is: one merged into the branch of the
    // copy it was built on is in the default branch only once that one is.
    if let (Some((number, pull)), Some(landed)) = (&pull, &copy.landed)
        && pull["merged_at"].is_string()
        && !landed.commit.is_empty()
        && (pull["base"]["ref"] == project.branch.as_str()
            || land::merged_pull(&github, &project.repo, landed, &project.branch)?.is_some())
    {
        let said = format!("pull request #{number} was merged; {name} lands next as a new one");
        return Ok(Some(Merged { commit: landed.commit.clone(), said, done: true, github }));
    }
    let Some(stacked) = &copy.stacked else { return Ok(None) };
    let Some(number) = land::merged_pull(&github, &project.repo, stacked, &project.branch)? else { return Ok(None) };
    let mut said = format!("{} was merged (#{number}); {name} now builds on {}", stacked.branch, project.branch);
    // GitHub closes a pull request whose base branch goes, and it can't be reopened.
    let closed = pull.as_ref().filter(|(_, pull)| pull["state"] != "open");
    if let Some((number, _)) = closed {
        said.push_str(&format!("; its pull request #{number} closed with that branch, so `gft land` opens a new one"));
    }
    Ok(Some(Merged { commit: stacked.commit.clone(), said, done: closed.is_some(), github }))
}

/// Move a copy onto the newest commit of the default branch, or the folder as it is
/// now. Says how many files have conflict markers, from this sync or an earlier one.
fn sync(store: &Store, repo: &str, name: &str) -> Result<usize> {
    let project = store.load(repo)?;
    let copy = copy(&project, name)?.clone();
    mount_copy(&copy)?;
    let merged = merged(&project, &copy, name)?;
    let saved = save_unless_saved(store, repo, name, "before sync")?;
    let synced = sync_saved(store, &project, &copy, name, merged.as_ref());
    // A snapshot of a copy the sync didn't touch only clutters the list. One that
    // failed may have changed files before it did, so its snapshot stays.
    if let Some(snapshot) = saved
        && matches!(synced, Ok(false))
    {
        drop_snapshot(store, repo, name, &snapshot)?;
    }
    synced?;
    // Conflicts left by this sync or an earlier one count until they are settled.
    let project = store.load(repo)?;
    let marked = marked(self::copy(&project, name)?, false)?;
    if !marked.is_empty() {
        let paths: Vec<&str> = marked.iter().map(String::as_str).collect();
        println!("conflict markers in: {}", paths.join(", "));
    }
    Ok(marked.len())
}

/// The rest of `sync`, once the copy is saved: whether it moved.
fn sync_saved(store: &Store, project: &Project, copy: &Copy, name: &str, merged: Option<&Merged>) -> Result<bool> {
    let repo = &project.repo;
    if !project.folder {
        set_aside_commits(&copy.dir, &copy.base)?;
    }
    // A copy of a folder keeps the folder as it took it until it moves: what it
    // started from there, for each file it changed, is read now to merge from.
    let folder_bases = match project.folder {
        true => folder_bases(copy)?,
        false => None,
    };
    // Moved in place, so programs working in the copy carry on.
    let updated = sandfs()?.update(&copy.workspace, None)?;
    let commit = match project.folder {
        true => None,
        false => Some(git(&copy.dir, &["rev-parse", "HEAD"])?),
    };
    if commit.as_ref() == Some(&copy.base) && updated.both.is_empty() {
        println!("{name} is up to date at {}", short(&copy.base));
        return Ok(false);
    }
    let new = store.edit(repo, |project| {
        let base = commit.clone().unwrap_or_else(|| folder_base(project));
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.base = base.clone();
        Ok(base)
    })?;
    let both = &updated.both;
    // Files upstream renamed that the copy changed under the old name, and not under
    // the new one, are merged under the new name.
    let renames = match project.folder || both.is_empty() {
        true => BTreeMap::new(),
        false => renames(&copy.dir, &copy.base, &new)?,
    };
    let (conflicts, looks) = take_in(both, &renames, |path, renamed| {
        Ok(match (project.folder, merged) {
            // From the folder as the copy took it, kept before it moved.
            // Only files: a link's target is no text to merge, nor a place to write.
            (true, _) => match &folder_bases {
                Some(bases) if regular(&copy.dir, path) && regular(&project.home, path) => {
                    let theirs = std::fs::read(project.home.join(path))?;
                    merge_bytes(&copy.dir, path, path, bases.get(path).cloned().flatten(), theirs)?
                }
                _ if entry(&project.home, path).is_none() => {
                    "deleted in the folder, changed here; kept yours".to_owned()
                }
                _ => "changed in the folder too; kept yours".to_owned(),
            },
            (false, Some(merged)) => {
                let base = land::file_at(&merged.github, &merged.commit, path)?;
                merge(&copy.dir, base, &new, path, renamed)?
            }
            (false, None) => {
                let base = tools::git_raw(&copy.dir, &["show", &format!("{}:{path}", copy.base)]).ok();
                merge(&copy.dir, base, &new, path, renamed)?
            }
        })
    })?;
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        // Markers left by an earlier sync move with a file upstream renamed.
        let moved: Vec<(String, String)> = copy
            .conflicts
            .iter()
            .filter_map(|old| renames.get(old).map(|new| (old.clone(), new.clone())))
            .filter(|(old, _)| std::fs::symlink_metadata(copy.dir.join(old)).is_err())
            .collect();
        for (old, new) in moved {
            copy.conflicts.remove(&old);
            copy.conflicts.insert(new);
        }
        copy.conflicts.extend(conflicts.iter().cloned());
        if let Some(merged) = merged {
            // That work is the default branch's now: the copy stands on that alone,
            // and what it lands next is new.
            copy.stacked = None;
            if merged.done {
                copy.pr = None;
                copy.landed = None;
                copy.pending = None;
            }
        }
        Ok(())
    })?;
    if let Some(merged) = merged {
        println!("{}", merged.said);
    }
    let moved = match project.folder {
        true => format!("{name} now has {} as it is", project.home.display()),
        false => format!("{name} moved from {} to {}", short(&copy.base), short(&new)),
    };
    println!("{moved}; {} changed on both sides, {looks} need a look", files(both.len()));
    Ok(true)
}

/// The bytes, none for no file, of each file the copy of a folder changed, as the
/// folder was when the copy took it; none when sandfs doesn't say where that is.
fn folder_bases(copy: &Copy) -> Result<Option<BTreeMap<String, Option<Vec<u8>>>>> {
    let Ok(taken) = taken_folder(copy) else { return Ok(None) };
    let mut bases = BTreeMap::new();
    for change in changes(&copy.workspace, None, &copy.dir)? {
        // Only a file's bytes: reading through a link could take anything, or forever.
        let data = regular(&taken, &change.path).then(|| std::fs::read(taken.join(&change.path)).ok()).flatten();
        bases.insert(change.path.clone(), data);
    }
    Ok(Some(bases))
}

/// Move the branch in the copy at `dir` back to its base `base`, past commits made
/// in the copy, keeping their files as they are: Graft lands files, not commits, and
/// a sync can't move a branch with commits of its own.
fn set_aside_commits(dir: &Path, base: &str) -> Result<()> {
    let head = git(dir, &["rev-parse", "HEAD"])?;
    if head == base || git(dir, &["merge-base", "--is-ancestor", base, &head]).is_err() {
        return Ok(());
    }
    let count = git(dir, &["rev-list", "--count", &format!("{base}..{head}")])?;
    git(dir, &["reset", "-q", "--soft", base])?;
    let commits = if count == "1" { "1 commit".to_owned() } else { format!("{count} commits") };
    println!("set aside {commits} made in the copy: their changes stay in its files, and `git reflog` has them");
    Ok(())
}

/// Write a copy's changes into the plain folder it is a copy of, refusing them all if
/// a file it changed was also changed there since, then move the copy onto the folder
/// as it is now, where its changes are the folder's own and no longer the copy's.
fn land_folder(store: &Store, project: &Project, name: &str, allow_conflicts: bool) -> Result<()> {
    let copy = copy(project, name)?.clone();
    mount_copy(&copy)?;
    refuse_conflicts(&marked(&copy, true)?, allow_conflicts)?;
    let changed = changes(&copy.workspace, None, &copy.dir)?;
    if changed.is_empty() {
        bail!("{name} has no changes to land");
    }
    copy_changes(&copy.workspace, None, &project.home).map_err(|err| {
        // Said by sandfs, whose way past it Graft doesn't offer: Graft's is a sync.
        let said = format!("{err:#}");
        match said.split_once("; pass --force") {
            Some((why, _)) => anyhow::anyhow!(
                "{}; `gft sync {name}` merges the folder's version in, then land again",
                why.replace("since the workspace started", &format!("since {name} took it"))
            ),
            None => err,
        }
    })?;
    sandfs()?.update(&copy.workspace, None)?;
    store.edit(&project.repo, |project| {
        let base = folder_base(project);
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.base = base;
        Ok(())
    })?;
    println!("{name}: {} written to {}", files(changed.len()), project.home.display());
    Ok(())
}

/// Take upstream's change to each of the files `both` sides changed, as `outcome_of`
/// does for a file and the name upstream renamed it to, printing what happened. Says
/// which files have conflict markers now and how many need a look.
fn take_in(
    both: &[String],
    renames: &BTreeMap<String, String>,
    mut outcome_of: impl FnMut(&str, Option<&str>) -> Result<String>,
) -> Result<(BTreeSet<String>, usize)> {
    let mut conflicts = BTreeSet::new();
    let mut looks = 0;
    for path in both {
        // Merged under the new name unless the copy changed that one too.
        let renamed = renames.get(path).map(String::as_str).filter(|to| !both.iter().any(|path| path == to));
        let outcome = outcome_of(path, renamed)?;
        match outcome.as_str() {
            "conflict" => _ = conflicts.insert(path.clone()),
            moved if moved.ends_with("; conflict there") => _ = conflicts.insert(renamed.unwrap_or(path).to_owned()),
            _ => {}
        }
        if outcome != "merged" && !outcome.ends_with("; merged there") {
            looks += 1;
        }
        println!("{outcome}\t{path}");
    }
    Ok((conflicts, looks))
}

/// Bring the home copy of a repository onto the newest commit of the default branch,
/// merging what was changed there as `sync` does for a copy. Says how many files
/// have conflict markers.
fn sync_home(project: &Project) -> Result<usize> {
    let home = &project.home;
    ensure_mounted(&project.workspace, home)?;
    let old = git(home, &["rev-parse", "HEAD"])?;
    let updated = sandfs()?.update(&project.workspace, None)?;
    let new = git(home, &["rev-parse", "HEAD"])?;
    // Markers left by this sync or an earlier one count until they are settled.
    let marked = || -> Result<usize> {
        let changed = changes(&project.workspace, None, home)?.into_iter().map(|change| change.path).collect();
        let marked = markers_in(home, &changed);
        if !marked.is_empty() {
            let paths: Vec<&str> = marked.iter().map(String::as_str).collect();
            println!("conflict markers in: {}", paths.join(", "));
        }
        Ok(marked.len())
    };
    if new == old && updated.both.is_empty() {
        println!("{} is up to date at {}", home_name(project), short(&old));
        return marked();
    }
    let both = &updated.both;
    let renames = if both.is_empty() { BTreeMap::new() } else { renames(home, &old, &new)? };
    let (_, looks) = take_in(both, &renames, |path, renamed| {
        let base = tools::git_raw(home, &["show", &format!("{old}:{path}")]).ok();
        merge(home, base, &new, path, renamed)
    })?;
    println!(
        "{} moved from {} to {}; {} changed on both sides, {looks} need a look",
        home_name(project),
        short(&old),
        short(&new),
        files(both.len())
    );
    marked()
}

/// Merge upstream's change to `path`, from `base` (what the copy started from there;
/// none for no file) to commit `new`, into the copy's version in `dir`, line by line,
/// leaving conflict markers where both changed the same lines. A file upstream
/// renamed to `renamed` is merged there, and leaves its old name. Says what happened.
fn merge(dir: &Path, base: Option<Vec<u8>>, new: &str, path: &str, renamed: Option<&str>) -> Result<String> {
    let meta = entry(dir, path);
    if !meta.as_ref().is_some_and(|meta| meta.is_file()) {
        let said = if meta.is_none() { "deleted here, changed upstream; kept deleted" } else { "kept yours" };
        return Ok(said.to_owned());
    }
    let show = |at: &str| tools::git_raw(dir, &["show", &format!("{new}:{at}")]).ok();
    let (target, theirs) = match (show(path), renamed) {
        (Some(theirs), _) => (path, theirs),
        // Only into a file, or nothing yet: never through a link or into a folder.
        (None, Some(to)) if under_link(dir, to) || entry(dir, to).is_some_and(|meta| !meta.is_file()) => {
            return Ok(format!("renamed upstream to {to}, which isn't a file here; kept yours"));
        }
        (None, Some(to)) => match show(to) {
            Some(theirs) => (to, theirs),
            None => return Ok("deleted upstream, changed here; kept yours".to_owned()),
        },
        (None, None) => return Ok("deleted upstream, changed here; kept yours".to_owned()),
    };
    merge_bytes(dir, path, target, base, theirs)
}

/// Merge `theirs` into the copy's `path` in `dir`, from `base`, writing the result to
/// `target`: `path`, or the name upstream renamed it to.
fn merge_bytes(dir: &Path, path: &str, target: &str, base: Option<Vec<u8>>, theirs: Vec<u8>) -> Result<String> {
    let mine = dir.join(path);
    let base = base.unwrap_or_default();
    let ours = std::fs::read(&mine)?;
    if [&ours, &base, &theirs].iter().any(|data| data.contains(&0)) {
        return Ok("binary, changed on both sides; kept yours".to_owned());
    }
    let tmp = std::env::temp_dir().join(format!("graft-merge-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let (base_file, theirs_file) = (tmp.join("base"), tmp.join("theirs"));
    std::fs::write(&base_file, &base)?;
    std::fs::write(&theirs_file, &theirs)?;
    let into = dir.join(target);
    if target != path {
        // The copy's version moves to the new name, with its mode, and is merged there.
        std::fs::copy(&mine, &into)?;
    }
    let result = merge_file(&into, &base_file, &theirs_file, target);
    let _ = std::fs::remove_dir_all(&tmp);
    let outcome = result?;
    if target == path {
        return Ok(outcome.to_owned());
    }
    std::fs::remove_file(&mine)?;
    Ok(format!("renamed upstream to {target}; {outcome} there"))
}

/// The files upstream renamed between commits `old` and `new`, by old name, as git
/// in the copy at `dir` finds them.
fn renames(dir: &Path, old: &str, new: &str) -> Result<BTreeMap<String, String>> {
    let listed = tools::git_raw(dir, &["diff", "--name-status", "-z", "-M", "--diff-filter=R", old, new])?;
    let mut fields = listed.split(|&byte| byte == 0).map(|field| String::from_utf8_lossy(field).into_owned());
    let mut renames = BTreeMap::new();
    while let (Some(_status), Some(from), Some(to)) = (fields.next(), fields.next(), fields.next()) {
        renames.insert(from, to);
    }
    Ok(renames)
}

fn merge_file(mine: &Path, base: &Path, theirs: &Path, path: &str) -> Result<&'static str> {
    let out = std::process::Command::new("git")
        .args(["merge-file", "-p", "-L", "yours", "-L", "base", "-L", "upstream"])
        .args([mine, base, theirs])
        .output()
        .context("running git merge-file")?;
    // Git caps the conflict count at 127. Negative errors appear as 255 on Unix,
    // so only documented merge results may replace the working file.
    match out.status.code() {
        Some(code @ 0..=127) => {
            std::fs::write(mine, &out.stdout)?;
            Ok(if code == 0 { "merged" } else { "conflict" })
        }
        _ => bail!("git merge-file failed on {path}: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

fn rm(store: &Store, repo: &str, name: &str) -> Result<()> {
    let project = store.load(repo)?;
    let copy = copy(&project, name)?;
    if mounted(&copy.workspace)?.is_some() {
        in_use(&copy.dir, sandfs()?.remove(&copy.workspace, true))?;
    } else {
        sandfs()?.remove(&copy.workspace, false)?;
    }
    for snapshot in &copy.snapshots {
        sandfs()?.remove(&snapshot.workspace, false)?;
    }
    // Empty once unmounted.
    let _ = std::fs::remove_dir(&copy.dir);
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
    fn merge_file_errors_leave_working_bytes_intact() {
        let dir = tempfile::tempdir().unwrap();
        let (mine, base, theirs) = (dir.path().join("mine"), dir.path().join("missing"), dir.path().join("theirs"));
        let ours = b"local changes\r\nincluding non-UTF-8: \xff\n";
        std::fs::write(&mine, ours).unwrap();
        std::fs::write(&theirs, b"upstream\n").unwrap();

        let result = merge_file(&mine, &base, &theirs, "notes.txt");

        assert_eq!(std::fs::read(&mine).unwrap(), ours);
        let error = result.unwrap_err();
        assert!(error.to_string().contains("git merge-file failed on notes.txt:"), "{error}");
    }

    #[test]
    fn merge_file_combines_non_overlapping_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (mine, base, theirs) = (dir.path().join("mine"), dir.path().join("base"), dir.path().join("theirs"));
        std::fs::write(&base, b"one\ntwo\nthree\nfour\nfive\n").unwrap();
        std::fs::write(&mine, b"ours\ntwo\nthree\nfour\nfive\n").unwrap();
        std::fs::write(&theirs, b"one\ntwo\nthree\nfour\nupstream\n").unwrap();

        assert_eq!(merge_file(&mine, &base, &theirs, "notes.txt").unwrap(), "merged");
        assert_eq!(std::fs::read(&mine).unwrap(), b"ours\ntwo\nthree\nfour\nupstream\n");
    }

    #[test]
    fn merge_file_keeps_real_conflicts_including_the_capped_count() {
        let dir = tempfile::tempdir().unwrap();
        let (mine, base, theirs) = (dir.path().join("mine"), dir.path().join("base"), dir.path().join("theirs"));
        for count in [1, 2, 127, 128] {
            let contents = |side: &str| (0..count).map(|n| format!("{side} {n}\n\na\nb\nc\n\n")).collect::<String>();
            std::fs::write(&base, contents("base")).unwrap();
            std::fs::write(&mine, contents("ours")).unwrap();
            std::fs::write(&theirs, contents("upstream")).unwrap();

            assert_eq!(merge_file(&mine, &base, &theirs, "notes.txt").unwrap(), "conflict");
            let merged = std::fs::read_to_string(&mine).unwrap();
            let expected = (0..count)
                .map(|n| format!("<<<<<<< yours\nours {n}\n=======\nupstream {n}\n>>>>>>> upstream\n\na\nb\nc\n\n"))
                .collect::<String>();
            assert_eq!(merged, expected, "{count} conflicts");
        }
    }

    #[test]
    fn nothing_under_a_link_counts_as_there() {
        let (a, b, outside) =
            (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(outside.path().join("file.txt"), "same\n").unwrap();
        std::fs::create_dir(b.path().join("src")).unwrap();
        std::fs::write(b.path().join("src/file.txt"), "same\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), a.path().join("src")).unwrap();
        assert!(under_link(a.path(), "src/file.txt") && !under_link(b.path(), "src/file.txt"));
        assert!(entry(a.path(), "src/file.txt").is_none() && entry(a.path(), "src").is_some());
        assert!(!regular(a.path(), "src/file.txt") && regular(b.path(), "src/file.txt"));
        assert!(!same_file(a.path(), b.path(), "src/file.txt"));
    }

    #[test]
    fn same_file_sees_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(a.path().join("run.sh"), "echo\n").unwrap();
        std::fs::write(b.path().join("run.sh"), "echo\n").unwrap();
        assert!(same_file(a.path(), b.path(), "run.sh"));
        std::fs::set_permissions(b.path().join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!same_file(a.path(), b.path(), "run.sh"));
    }

    #[test]
    fn folder_bases_differ_whatever_the_clock_says() {
        let mut project = Project {
            repo: "folder/notes".into(),
            source: "/notes".into(),
            home: "/notes".into(),
            workspace: "notes".into(),
            branch: String::new(),
            folder: true,
            checkout: false,
            takes: 0,
            fetched: 0,
            copies: BTreeMap::new(),
        };
        let (a, b) = (folder_base(&mut project), folder_base(&mut project));
        assert_ne!(a, b);
        assert!(a.starts_with("1@") && b.starts_with("2@"), "{a} {b}");
        assert!(now() - copied_at(&a).unwrap() <= 1);
        assert_eq!(copied_at("7@1700000000"), Some(1_700_000_000));
    }

    #[test]
    fn times_are_told_in_the_largest_unit() {
        assert_eq!(ago(0), "0 sec ago");
        assert_eq!(ago(59), "59 sec ago");
        assert_eq!(ago(180), "3 min ago");
        assert_eq!(ago(7300), "2 hr ago");
        assert_eq!(ago(3 * 86400), "3 days ago");
    }

    #[test]
    fn repositories_are_named_by_owner_and_name() {
        let parse = |repo: &str| parse_repo(repo, None).unwrap();
        assert_eq!(parse("lydakis/sandfs"), ("lydakis/sandfs".into(), "https://github.com/lydakis/sandfs".into()));
        assert_eq!(parse("lydakis/sandfs.git").0, "lydakis/sandfs");
        assert_eq!(parse("lydakis/sandfs/").0, "lydakis/sandfs");
        for url in [
            "https://github.com/lydakis/sandfs",
            "https://github.com/lydakis/sandfs.git",
            "https://github.com/lydakis/sandfs/",
            "https://github.com/lydakis/sandfs.git/",
            "http://github.com/lydakis/sandfs",
            "https://www.github.com/lydakis/sandfs",
            "git@github.com:lydakis/sandfs.git",
            "git@github.com:lydakis/sandfs",
            "ssh://git@github.com/lydakis/sandfs",
            "ssh://git@github.com:22/lydakis/sandfs.git",
        ] {
            assert_eq!(parse(url), ("lydakis/sandfs".into(), url.into()), "{url}");
        }
    }

    #[test]
    fn other_hosts_need_to_say_where_copies_land() {
        let other = "https://gitlab.com/lydakis/sandfs";
        assert!(parse_repo(other, None).is_err());
        assert_eq!(parse_repo(other, Some("o/n".into())).unwrap(), ("o/n".into(), other.into()));
        assert!(parse_repo("file:///srv/sandfs", None).is_err());
        assert_eq!(parse_repo("file:///srv/sandfs", Some("o/n".into())).unwrap().0, "o/n");
        // github.com in the path or in a longer host name is not GitHub.
        assert!(parse_repo("https://example.com/github.com/lydakis/sandfs", None).is_err());
        assert!(parse_repo("https://github.com.example.com/lydakis/sandfs", None).is_err());
        // Plain OWNER/NAME mounts from GitHub and lands wherever --github says.
        assert_eq!(parse_repo("a/b", Some("o/n".into())).unwrap(), ("o/n".into(), "https://github.com/a/b".into()));
    }

    #[test]
    fn bad_repositories_are_refused() {
        for repo in [
            "",
            "sandfs",
            "/",
            "a/b/c",
            "/a/b",
            "./a",
            "../a",
            "a/",
            "/b",
            "a b/c",
            "a/.git",
            "github.com:a/b",
            "https://github.com/sandfs",
            "https://github.com/",
            "https://github.com",
            "https://github.com/a/b/tree/main",
            "git@github.com:sandfs.git",
            "git@github.com",
        ] {
            assert!(parse_repo(repo, None).is_err(), "{repo:?} should be refused");
        }
        assert!(parse_repo("a/b", Some("nope".into())).is_err());
        assert!(parse_repo("https://gitlab.com/a/b", Some("a/b/c".into())).is_err());
    }
}
// bench
// again 1
// again 2
// again 3
