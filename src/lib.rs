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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::state::{Copy, Project, Snapshot, Store, check_copy_name};
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
        /// Start from this copy instead, with its changes.
        #[arg(long, value_name = "COPY")]
        from: Option<String>,
    },
    /// List the copies: how many files each changed, the commit it sits on, its pull
    /// request, and which other copies changed the same files.
    Ls,
    /// Describe the copy the current folder is in, or the project from its home copy.
    Status,
    /// Print the folder of a copy.
    Path { name: String },
    /// List the files a copy changed (A, M, D), or how copy A differs from copy B.
    Changes {
        a: Option<String>,
        b: Option<String>,
        /// Print only the totals, like `3 added, 5 modified, 1 deleted`.
        #[arg(long)]
        count: bool,
    },
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
    },
}

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
        Cmd::Status => status(&store),
        Cmd::Path { name } => {
            let (project, _) = here(&store)?;
            println!("{}", copy(&project, &name)?.dir.display());
            Ok(())
        }
        Cmd::Changes { a, b, count } => {
            let (project, current) = here(&store)?;
            let workspace = |name: Option<String>| -> Result<(String, PathBuf)> {
                match name.or(current.clone()) {
                    Some(name) => {
                        let copy = copy(&project, &name)?;
                        Ok((copy.workspace.clone(), copy.dir.clone()))
                    }
                    None if project.folder => bail!("name a copy: the folder itself is what copies change"),
                    None => Ok((project.workspace.clone(), project.home.clone())),
                }
            };
            let (a, dir) = workspace(a)?;
            let b = b.map(|b| workspace(Some(b)).map(|(b, _)| b)).transpose()?;
            let changes = changes(&a, b.as_deref(), &dir)?;
            if count {
                let of = |kind| changes.iter().filter(|change| change.kind == kind).count();
                println!("{} added, {} modified, {} deleted", of('A'), of('M'), of('D'));
            } else {
                for change in changes {
                    println!("{} {}", change.kind, change.path);
                }
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
            sync(&store, &project.repo, &which(name, current)?)
        }
        Cmd::Land { name, message, body, body_file, draft, force, dry_run } => {
            let (project, current) = here(&store)?;
            if project.folder {
                if message.is_some() || body.is_some() || body_file.is_some() || draft || force || dry_run {
                    bail!(
                        "a copy of a folder lands into the folder; --message, --body, --draft, --force and \
                         --dry-run are for pull requests"
                    );
                }
                return land_folder(&store, &project, &which(name, current)?);
            }
            let body = match body_file {
                Some(path) => Some(read_body(&path)?),
                None => body,
            };
            let options = land::Options { message: message.as_deref(), body: body.as_deref(), draft, force, dry_run };
            land::land(&store, &project.repo, &which(name, current)?, &options)
        }
        Cmd::Rm { names, force } => {
            let (project, _) = here(&store)?;
            leave_mounts()?;
            // All checked before any is deleted, so a refusal leaves every copy as it was.
            let mut kept = Vec::new();
            for name in names.iter().filter(|_| !force) {
                let lost = unlanded(copy(&project, name)?)?;
                if lost > 0 {
                    kept.push(format!("{name} has {} that never landed", files(lost)));
                }
            }
            if !kept.is_empty() {
                bail!("{}; `gft land` them first, or `gft rm --force` to delete them anyway", kept.join(", "));
            }
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
        if dir.is_some() || github.is_some() {
            bail!("a folder is its own home copy; DIR and --github are for repositories");
        }
        return get_folder(store, &folder);
    }
    if repo.starts_with(['/', '.', '~']) {
        bail!("{repo} is no folder");
    }
    let (slug, source) = parse_repo(repo, github)?;
    let name = slug.rsplit('/').next().expect("owner/name").to_owned();
    let dir = std::path::absolute(dir.unwrap_or_else(|| PathBuf::from(&name)))?;
    if store.exists(&slug) {
        bail!("{slug} is already a project here; its home copy is listed by `gft ls`");
    }
    mount(&source, &dir, &name)?;
    let dir = dir.canonicalize()?;
    let mut project = Project {
        repo: slug.clone(),
        source,
        home: dir.clone(),
        workspace: name.clone(),
        branch: String::new(),
        folder: false,
        takes: 0,
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
        takes: 0,
        copies: BTreeMap::new(),
    };
    store.create(&project)?;
    println!("{}: a plain folder; `gft new NAME` makes a copy of it next to it", folder.display());
    Ok(())
}

fn new(store: &Store, names: &[String], from: Option<&str>) -> Result<()> {
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
    // A copy of a copy that landed sits on that landing, as it is now.
    let mut stacked = None;
    let (source, mut base) = match from {
        Some(from) => {
            let from = copy(&project, from)?;
            // A landing kept by Graft before 0.1 doesn't say its commit until that copy
            // lands again, so nothing is built on it until then.
            stacked = from.landed.clone().or_else(|| from.stacked.clone()).filter(|landed| !landed.commit.is_empty());
            if let Some(stacked) = &mut stacked {
                stacked.files.clear();
            }
            (from.workspace.clone(), Some(from.base.clone()))
        }
        // Each copy is the folder as it is when made.
        None if project.folder => (project.source.clone(), None),
        None => {
            // Brings the default branch up to date once for all the copies made now.
            let main = main_workspace(&project);
            sandfs()?.update(&main, None)?;
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
                from: from.map(str::to_owned),
                stacked: stacked.clone(),
                landed: None,
                pending: None,
                pr: None,
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
fn ls(store: &Store) -> Result<()> {
    match here(store) {
        Ok((project, _)) => list(&project),
        Err(err) => {
            let mut projects = store.projects()?;
            if projects.is_empty() {
                return Err(err);
            }
            projects.sort_by(|a, b| a.repo.cmp(&b.repo));
            projects.iter().try_for_each(list)
        }
    }
}

fn list(project: &Project) -> Result<()> {
    let kind = if project.folder { "folder".to_owned() } else { format!("home, {}", project.branch) };
    println!("{}\t{}\t({kind})", project.workspace, project.home.display());
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
        let files = files(mine.len());
        println!("{name}\t{files}\t{}\t{pr}\t{}", based(project, &copy.base), notes.join("; "));
    }
    Ok(())
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

fn status(store: &Store) -> Result<()> {
    let (project, current) = here(store)?;
    let Some(name) = current else {
        println!("project  {}", project.repo);
        println!("folder   {}", project.home.display());
        println!("copies   {}", project.copies.len());
        return Ok(());
    };
    let copy = copy(&project, &name)?;
    let files = changes(&copy.workspace, None, &copy.dir)?.len();
    println!("copy       {name}");
    println!("folder     {}", copy.dir.display());
    match project.folder {
        true => println!("copied     {}", ago(now().saturating_sub(copied_at(&copy.base).unwrap_or(0)))),
        false => println!("on commit  {}", short(&copy.base)),
    }
    println!("changed    {files} {}", if files == 1 { "file" } else { "files" });
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
        copy_changes(&target.workspace, Some(&copy.workspace), &copy.dir)?;
    } else {
        // A snapshot from before a sync sits on another commit: the copy is made again.
        in_use(copy, sandfs()?.remove(&copy.workspace, true))?;
        fork(&target.workspace, Some(&copy.dir), &copy.workspace)?;
    }
    // Snapshots newer than the one gone back to go; that one stays, to go back to again.
    for later in &copy.snapshots[keep + 1..] {
        sandfs()?.remove(&later.workspace, false)?;
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
    mount_copy(&copy)?;
    // Moved in place, so programs working in the copy carry on.
    let updated = sandfs()?.update(&copy.workspace, None)?;
    let commit = match project.folder {
        true => None,
        false => Some(git(&copy.dir, &["rev-parse", "HEAD"])?),
    };
    let new = store.edit(repo, |project| {
        let base = commit.clone().unwrap_or_else(|| folder_base(project));
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.base = base.clone();
        Ok(base)
    })?;
    let both = &updated.both;
    let mut conflicts = 0;
    for path in both {
        // A folder keeps no older version to merge from.
        let outcome = match project.folder {
            true => "changed in the folder too; kept yours",
            false => merge(&copy.dir, &copy.base, &new, path)?,
        };
        if outcome != "merged" {
            conflicts += 1;
        }
        println!("{outcome}\t{path}");
    }
    let moved = match project.folder {
        true => format!("{name} now has {} as it is", project.home.display()),
        false => format!("{name} moved from {} to {}", short(&copy.base), short(&new)),
    };
    println!("{moved}; {} files changed on both sides, {conflicts} need a look", both.len());
    Ok(())
}

/// Write a copy's changes into the plain folder it is a copy of, refusing them all if
/// a file it changed was also changed there since, then move the copy onto the folder
/// as it is now, where its changes are the folder's own and no longer the copy's.
fn land_folder(store: &Store, project: &Project, name: &str) -> Result<()> {
    let copy = copy(project, name)?.clone();
    mount_copy(&copy)?;
    let changed = changes(&copy.workspace, None, &copy.dir)?;
    if changed.is_empty() {
        bail!("{name} has no changes to land");
    }
    copy_changes(&copy.workspace, None, &project.home)?;
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
    let result = merge_file(&mine, &base_file, &theirs_file, path);
    let _ = std::fs::remove_dir_all(&tmp);
    result
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
        in_use(copy, sandfs()?.remove(&copy.workspace, true))?;
    } else {
        sandfs()?.remove(&copy.workspace, false)?;
    }
    for snapshot in &copy.snapshots {
        sandfs()?.remove(&snapshot.workspace, false)?;
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
    fn folder_bases_differ_whatever_the_clock_says() {
        let mut project = Project {
            repo: "folder/notes".into(),
            source: "/notes".into(),
            home: "/notes".into(),
            workspace: "notes".into(),
            branch: String::new(),
            folder: true,
            takes: 0,
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
