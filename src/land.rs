//! `gft land`: a copy's changes become a commit on a branch on GitHub, and a pull
//! request, made through GitHub's API from the files themselves. No local commit or
//! push is involved, so the copy needs no history and git in it is never asked.
//!
//! The commit's tree is the copy's base commit's tree with the copy's changes laid
//! over it, so it is always the copy's whole state. Its first parent is the commit
//! the copy's last landing left the branch at; on a first landing, the landing the
//! copy was made on top of, if any, else its base. After `gft sync` the new base
//! becomes a second parent, so the pull request shows only the copy's own changes.
//!
//! Each step is recorded as soon as it is done, so a landing cut short (a lost
//! connection, GitHub refusing the pull request) is finished by landing again. A
//! branch that moved on GitHub in a way the copy can't account for is left alone:
//! the copy doesn't have those commits' changes, and landing over them would undo
//! them.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::github::GitHub;
use crate::state::{Landing, Pr, Store};
use crate::tools::{self, changes, git, short};

pub struct Options<'a> {
    pub message: Option<&'a str>,
    pub body: Option<&'a str>,
    pub draft: bool,
    /// Land even over commits on the branch that the copy didn't land.
    pub force: bool,
    /// Say what landing would do, changing nothing.
    pub dry_run: bool,
    /// Open a new pull request rather than add to the copy's earlier one.
    pub new_pr: bool,
    /// Land files that still have conflict markers from a sync.
    pub allow_conflicts: bool,
}

pub fn land(store: &Store, repo: &str, name: &str, options: &Options) -> Result<()> {
    let project = store.load(repo)?;
    let mut copy = crate::copy(&project, name)?.clone();
    crate::mount_copy(&copy)?;
    // Conflict markers a sync left are no change anyone means to publish.
    let marked = crate::marked(&copy, true)?;
    if !options.dry_run && marked != copy.conflicts {
        record(store, repo, name, |copy| copy.conflicts = marked.clone())?;
    }
    if !marked.is_empty() && !options.allow_conflicts {
        let paths: Vec<&str> = marked.iter().map(String::as_str).collect();
        bail!(
            "{} still {} conflict markers from `gft sync`: {}. Settle them first, or land with \
             --allow-conflicts to publish them as they are",
            crate::files(marked.len()),
            if marked.len() == 1 { "has" } else { "have" },
            paths.join(", ")
        );
    }
    let github = GitHub::new(repo)?;
    // Built on work merged since: its files still have that work as it was, and sync
    // takes in the merged version.
    if let Some(stacked) = &copy.stacked
        && let Some(merged) = merged_pull(&github, repo, stacked, &project.branch)?
    {
        bail!(
            "{} was merged (pull request #{merged}); `gft sync {name}` to move onto {} with it, then land",
            stacked.branch,
            project.branch
        );
    }
    // The pull request as GitHub has it now. One merged, or closed, takes no more
    // commits: the copy lands afresh, on a branch and a pull request of its own.
    let mut open_base = None;
    if let Some(pr) = copy.pr.clone() {
        let pull = github.call("GET", &format!("pulls/{}", pr.number), None)?;
        let merged = pull["merged"].as_bool() == Some(true) || pull["merged_at"].is_string();
        let open = pull["state"] == "open";
        if open && !options.new_pr {
            open_base = pull["base"]["ref"].as_str().map(str::to_owned);
        } else {
            if merged && pull["base"]["ref"] != project.branch.as_str() {
                let into = pull["base"]["ref"].as_str().unwrap_or("another branch");
                bail!(
                    "pull request #{} was merged into {into}, not {}: once {into} is merged, `gft sync {name}` \
                     moves {name} onto {} with both, and it lands again as a new pull request",
                    pr.number,
                    project.branch,
                    project.branch
                );
            }
            if merged {
                let merge = pull["merge_commit_sha"].as_str().unwrap_or_default();
                if !merge.is_empty() && !contains(&github, &copy.base, merge)? {
                    bail!(
                        "pull request #{} was merged; `gft sync {name}` first, so what lands next goes on top of it",
                        pr.number
                    );
                }
                println!("pull request #{} was merged; landing {name} as a new one", pr.number);
            } else if !open && !options.new_pr {
                bail!(
                    "pull request #{} was closed without being merged. Reopen it on GitHub and land again, or \
                     land with --new-pr to open another",
                    pr.number
                );
            } else {
                println!("leaving pull request #{} as it is; landing {name} as a new one", pr.number);
            }
            copy.pr = None;
            copy.landed = None;
            copy.pending = None;
            if !options.dry_run {
                record(store, repo, name, |copy| {
                    copy.pr = None;
                    copy.landed = None;
                    copy.pending = None;
                })?;
            }
        }
    }
    // The branch it lands on, and what that branch's pull request is based on: the
    // branch of the landing it was made on top of, else the default branch.
    let mut branch = match (&copy.pending, &copy.landed) {
        (Some(landing), _) | (None, Some(landing)) => landing.branch.clone(),
        (None, None) => format!("graft/{name}"),
    };
    let pr_base = copy.stacked.as_ref().map_or(project.branch.clone(), |stacked| stacked.branch.clone());
    let mut head = head_of(&github, &branch)?;

    // A landing cut short after its commit was made: if the branch moved to it, it
    // landed; if the branch is still where it was, it never did, and is made afresh.
    if let Some(pending) = copy.pending.take() {
        if head.as_ref() == Some(&pending.commit) {
            copy.landed = Some(pending);
        } else if head.as_ref() != copy.landed.as_ref().map(|landed| &landed.commit) {
            copy.pending = Some(pending);
        }
    }
    // Graft before 0.1 didn't keep the commit its landing left the branch at.
    if let (Some(landed), Some(head)) = (&mut copy.landed, &head)
        && landed.commit.is_empty()
    {
        landed.commit = head.clone();
    }
    // A first landing onto a branch that exists already, left by an earlier copy of
    // the same name or by anyone else, takes a name of its own instead of their history.
    // `--new-pr` always does, `--force` or not: the earlier pull request stays as it is.
    if copy.landed.is_none() && head.is_some() && (!options.force || options.new_pr) {
        copy.pending = None;
        let mut n = 2;
        while head.is_some() {
            branch = format!("graft/{name}-{n}");
            head = head_of(&github, &branch)?;
            n += 1;
        }
    }
    // The branch's last commit is this copy's own only when its last landing left it
    // there. Matching files don't make it so: someone else's branch stays theirs.
    let ours = copy.landed.as_ref().filter(|landed| head.as_ref() == Some(&landed.commit));
    if let Some(head) = &head
        && ours.is_none()
        && !options.force
    {
        let state = match &copy.landed {
            Some(landed) => {
                format!("has moved since {name} last landed (from {} to {})", short(&landed.commit), short(head))
            }
            None => format!("already exists, at {}, with commits {name} didn't land", short(head)),
        };
        bail!(
            "{branch} on GitHub {state}; landing now would undo what they changed. Take their changes into \
             {name} first, or land with --force to replace them"
        );
    }

    let changed = changes(&copy.workspace, None, &copy.dir)?;
    // A copy that landed and was then put back as its base has it lands all the
    // same, while its branch is there: the branch should lose those changes too.
    if changed.is_empty() && (copy.landed.is_none() || head.is_none()) {
        bail!("{name} has no changes to land");
    }
    // The first parent, and the base commit it was made on, when known.
    let (first, first_on) = match (&head, &copy.stacked) {
        // Over someone else's commits (with --force): made on what it last landed on, if anything.
        (Some(head), _) => (head.clone(), ours.or(copy.landed.as_ref()).map(|landed| landed.on.clone())),
        (None, Some(stacked)) => (stacked.commit.clone(), Some(stacked.on.clone())),
        (None, None) => (copy.base.clone(), Some(copy.base.clone())),
    };
    let mut parents = vec![first];
    // Made on a newer base than that parent: the base joins in.
    if first_on.as_ref() != Some(&copy.base) {
        parents.push(copy.base.clone());
    }
    // A copy of a checkout sits on the checkout's commit, which may not be pushed.
    let base_tree = match github.get_opt(&format!("git/commits/{}", copy.base))? {
        Some(commit) => {
            commit["tree"]["sha"].as_str().map(str::to_owned).context("GitHub gave a commit without a tree")?
        }
        None => bail!("{name} sits on commit {}, which GitHub doesn't have: push it first", short(&copy.base)),
    };
    if options.dry_run {
        let pr = copy.pr.as_ref().filter(|_| head.is_some());
        return preview(name, &branch, &pr_base, head.as_deref(), &parents, pr, &changed, ours.is_none());
    }
    // The description is kept in step with the one given, and always ends with where
    // the pull request came from.
    let footer = format!("Landed with `gft land` from the copy `{name}`.");
    let given = options.body.is_some();
    let body = match options.body.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{text}\n\n{footer}"),
        None => footer,
    };
    let describe = |number: u64| github.call("PATCH", &format!("pulls/{number}"), Some(&json!({"body": body})));
    // A pull request that goes against another branch than it should, as one built
    // on work merged since, goes against the right one.
    let retarget = |number: u64| -> Result<bool> {
        match &open_base {
            Some(base) if *base != pr_base => {
                github.call("PATCH", &format!("pulls/{number}"), Some(&json!({"base": pr_base})))?;
                Ok(true)
            }
            _ => Ok(false),
        }
    };

    let mut entries = Vec::new();
    let mut files = BTreeMap::new();
    for change in &changed {
        // Before reading the file: if it changes in between, it counts as not landed.
        files.insert(change.path.clone(), fingerprint(&copy.dir, &change.path)?);
        entries.push(match change.kind {
            'D' => json!({"path": change.path, "mode": "100644", "type": "blob", "sha": null}),
            _ => {
                let (mode, data) = read(&copy.dir.join(&change.path))?;
                let blob = github.call(
                    "POST",
                    "git/blobs",
                    Some(&json!({"content": base64::engine::general_purpose::STANDARD.encode(data),
                        "encoding": "base64"})),
                )?;
                json!({"path": change.path, "mode": mode, "type": "blob", "sha": sha(&blob)?})
            }
        });
    }
    let tree = match entries.is_empty() {
        true => base_tree,
        false => sha(&github.call("POST", "git/trees", Some(&json!({"base_tree": base_tree, "tree": entries})))?)?,
    };
    // The branch holds the copy as it is already, as after a landing whose pull
    // request was never made.
    let current = match ours {
        Some(ours) => commit_tree(&github, &ours.commit)? == tree,
        None => false,
    };
    // A branch gone from GitHub (deleted once its pull request was merged or closed)
    // is made again, with a pull request of its own.
    let pr = copy.pr.clone().filter(|_| head.is_some());

    let commit = match ours.filter(|_| current) {
        Some(ours) => {
            let ours = Landing { files, ..ours.clone() };
            let commit = ours.commit.clone();
            record(store, repo, name, |copy| {
                copy.pending = None;
                copy.landed = Some(ours);
                copy.pr = pr.clone();
            })?;
            commit
        }
        None => {
            let commit_message = match (options.message, &head) {
                (Some(message), _) => message.to_owned(),
                (None, None) => format!("Changes from {name}"),
                (None, Some(_)) => format!("Update {name}"),
            };
            let commit = github.call(
                "POST",
                "git/commits",
                Some(&json!({"message": commit_message, "tree": tree, "parents": parents})),
            )?;
            let commit = sha(&commit)?;
            // Recorded before the branch moves, so a landing cut short from here on
            // can tell its own commit on the branch from someone else's.
            let landing = Landing { branch: branch.clone(), commit: commit.clone(), on: copy.base.clone(), files };
            record(store, repo, name, |copy| copy.pending = Some(landing.clone()))?;
            match head {
                Some(_) => github.call("PATCH", &format!("git/refs/heads/{branch}"), Some(&json!({"sha": commit})))?,
                None => github.call(
                    "POST",
                    "git/refs",
                    Some(&json!({"ref": format!("refs/heads/{branch}"), "sha": commit})),
                )?,
            };
            record(store, repo, name, |copy| {
                copy.pending = None;
                copy.landed = Some(landing);
                copy.pr = pr.clone();
            })?;
            commit
        }
    };

    let pr = match pr {
        Some(pr) if current => {
            // Only the description, or what the pull request goes against, is new.
            let moved = retarget(pr.number)?;
            if given {
                describe(pr.number)?;
                println!("{branch} already has {name} as it is; described it anew: {}", pr.url);
                return Ok(());
            }
            if moved {
                println!(
                    "{branch} already has {name} as it is; its pull request now goes against {pr_base}: {}",
                    pr.url
                );
                return Ok(());
            }
            bail!("nothing new to land: {branch} already has {name} as it is");
        }
        Some(pr) => {
            if given {
                describe(pr.number)?;
            }
            retarget(pr.number)?;
            pr
        }
        // One made by a landing whose answer never came back is found, not made twice.
        None => match open_pr(&github, repo, &branch)? {
            Some(pr) => {
                if given {
                    describe(pr.number)?;
                }
                pr
            }
            None => {
                let title = options.message.map_or_else(|| name.to_owned(), str::to_owned);
                let made = github.call(
                    "POST",
                    "pulls",
                    Some(&json!({"title": title, "head": branch, "base": pr_base, "body": body,
                        "draft": options.draft})),
                )?;
                as_pr(&made)?
            }
        },
    };
    let url = pr.url.clone();
    record(store, repo, name, |copy| copy.pr = Some(pr))?;
    println!("{branch} at {}, {}: {url}", short(&commit), crate::files(changed.len()));
    Ok(())
}

/// What landing would publish: the branch and where it moves from, the commit's
/// parents, the pull request, and the files.
#[allow(clippy::too_many_arguments)]
fn preview(
    name: &str,
    branch: &str,
    pr_base: &str,
    head: Option<&str>,
    parents: &[String],
    pr: Option<&Pr>,
    changed: &[crate::tools::Change],
    replaces: bool,
) -> Result<()> {
    let moves = match head {
        None => "made new".to_owned(),
        Some(head) if replaces => format!("moved from {}, replacing commits {name} didn't land", short(head)),
        Some(head) => format!("moved from {}", short(head)),
    };
    let parents: Vec<&str> = parents.iter().map(|parent| short(parent)).collect();
    println!("branch        {branch} ({moves})");
    println!("commit        on {}", parents.join(" and "));
    match pr {
        Some(pr) => println!("pull request  {} (updated)", pr.url),
        None => println!("pull request  new or open one, against {pr_base}"),
    }
    println!("files         {}", crate::files(changed.len()));
    for change in changed {
        println!("{} {}", change.kind, change.path);
    }
    Ok(())
}

/// Change the copy `name` as kept on disk.
fn record(store: &Store, repo: &str, name: &str, change: impl FnOnce(&mut crate::state::Copy)) -> Result<()> {
    store.edit(repo, |project| {
        change(project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?);
        Ok(())
    })
}

/// The commit the branch `branch` is at on GitHub, if it exists.
fn head_of(github: &GitHub, branch: &str) -> Result<Option<String>> {
    github
        .get_opt(&format!("git/ref/heads/{branch}"))?
        .map(|found| found["object"]["sha"].as_str().map(str::to_owned).context("GitHub gave a branch without a sha"))
        .transpose()
}

/// The number of the pull request that carried `landing` into the branch `into` (the
/// default branch), if one did: one from its branch whose last commit has the
/// landing's. A branch name used again after an earlier pull request from it was
/// merged doesn't count, nor does one merged into another copy's branch, whose work
/// reaches `into` only once that one does.
pub fn merged_pull(github: &GitHub, repo: &str, landing: &Landing, into: &str) -> Result<Option<u64>> {
    let owner = repo.split('/').next().expect("owner/name");
    let found = github.call("GET", &format!("pulls?head={owner}:{}&state=closed", landing.branch), None)?;
    for pull in found.as_array().map(Vec::as_slice).unwrap_or_default() {
        let head = pull["head"]["sha"].as_str().unwrap_or_default();
        if pull["merged_at"].is_string()
            && pull["base"]["ref"] == into
            && !head.is_empty()
            && contains(github, head, &landing.commit)?
        {
            return Ok(pull["number"].as_u64());
        }
    }
    Ok(None)
}

/// Whether `commit` has `ancestor` in its history, as GitHub sees it. A commit
/// GitHub doesn't have has nothing.
fn contains(github: &GitHub, commit: &str, ancestor: &str) -> Result<bool> {
    let compared = github.get_opt(&format!("compare/{ancestor}...{commit}"))?;
    Ok(compared.is_some_and(|compared| matches!(compared["status"].as_str(), Some("identical" | "ahead"))))
}

/// The bytes of `path` in `commit` on GitHub, none when the commit has no file there.
pub fn file_at(github: &GitHub, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
    let encoded: String = path
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect();
    let Some(found) = github.get_opt(&format!("contents/{encoded}?ref={commit}"))? else { return Ok(None) };
    if found["type"] != "file" {
        return Ok(None);
    }
    // Over a megabyte, the contents come by themselves.
    let found = match found["content"].as_str() {
        Some(content) if !content.is_empty() => found,
        _ => github.call("GET", &format!("git/blobs/{}", sha(&found)?), None)?,
    };
    let content: String = found["content"].as_str().unwrap_or_default().split_whitespace().collect();
    Ok(Some(base64::engine::general_purpose::STANDARD.decode(content).context("GitHub sent unreadable contents")?))
}

/// The open pull request from `branch`, if there is one.
fn open_pr(github: &GitHub, repo: &str, branch: &str) -> Result<Option<Pr>> {
    let owner = repo.split('/').next().expect("owner/name");
    let found = github.call("GET", &format!("pulls?head={owner}:{branch}&state=open"), None)?;
    match found.as_array().map(Vec::as_slice).unwrap_or_default() {
        [] => Ok(None),
        [pull] => as_pr(pull).map(Some),
        _ => bail!("{branch} has more than one open pull request; close all but one and land again"),
    }
}

fn as_pr(pull: &Value) -> Result<Pr> {
    Ok(Pr::new(
        pull["number"].as_u64().context("GitHub gave no pull request number")?,
        pull["html_url"].as_str().unwrap_or_default().to_owned(),
    ))
}

/// The id of the tree of `commit`.
fn commit_tree(github: &GitHub, commit: &str) -> Result<String> {
    let commit = github.call("GET", &format!("git/commits/{commit}"), None)?;
    commit["tree"]["sha"].as_str().map(str::to_owned).context("GitHub gave a commit without a tree")
}

fn sha(value: &Value) -> Result<String> {
    value["sha"].as_str().map(str::to_owned).context("GitHub's answer has no sha")
}

/// The git mode and the contents of the file or link at `path`.
fn read(path: &Path) -> Result<(&'static str, Vec<u8>)> {
    let (mode, link) = mode(path)?;
    match link {
        Some(target) => Ok((mode, target)),
        None => Ok((mode, std::fs::read(path).with_context(|| format!("reading {}", path.display()))?)),
    }
}

/// The git mode of the file or link at `path`, and a link's target.
fn mode(path: &Path) -> Result<(&'static str, Option<Vec<u8>>)> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        return Ok(("120000", Some(target.as_os_str().as_encoded_bytes().to_vec())));
    }
    Ok((if meta.permissions().mode() & 0o111 != 0 { "100755" } else { "100644" }, None))
}

/// What the file at `path` in the copy looks like, so `gft rm` can tell whether it is
/// still what landed: `deleted` when there is no file there, else the file's own mode
/// and the git hash of its bytes as landing sends them, without the repository's
/// filters. Not its modification time or the copy's base: `gft sync` rewrites times
/// and moves the base without touching what the copy changed, and judging by either
/// would call a landed copy unlanded (or, the other way, miss an edit made inside the
/// same second as the landing). A file that `gft sync` merged upstream lines into
/// reads as changed, which it is: landing again would add a commit.
pub fn fingerprint(dir: &Path, path: &str) -> Result<String> {
    // No file there: nothing at the path, a folder, or a file or link in place of a
    // folder above it, as when a link replaced a folder of the copy's base.
    for at in Path::new(path).ancestors().filter(|at| !at.as_os_str().is_empty()) {
        let full = dir.join(at);
        let is_file = match std::fs::symlink_metadata(&full) {
            Ok(meta) => !meta.is_dir(),
            Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                return Ok("deleted".to_owned());
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", full.display())),
        };
        if is_file != (at == Path::new(path)) {
            return Ok("deleted".to_owned());
        }
    }
    let full = dir.join(path);
    let (mode, link) = mode(&full)?;
    let hash = match link {
        // In hex: a link's target is any bytes, and two differing ones must not read the same.
        Some(target) => target.iter().map(|byte| format!("{byte:02x}")).collect(),
        None => git(dir, &["hash-object", "--no-filters", "--", path])?,
    };
    Ok(format!("{mode} {hash}"))
}

/// What `fingerprint` said of `path` in commit `commit`, read from git in the copy at
/// `dir`, with no file's contents fetched but a link's.
pub fn fingerprint_at(dir: &Path, commit: &str, path: &str) -> Result<String> {
    let listed = git(dir, &["ls-tree", "-z", commit, "--", path])?;
    // `MODE TYPE ID\tPATH`; nothing when the commit has no file there.
    let Some((mode, rest)) = listed.trim_end_matches('\0').split_once(' ') else { return Ok("deleted".to_owned()) };
    let Some(("blob", id)) = rest.split_once('\t').and_then(|(about, _)| about.split_once(' ')) else {
        return Ok("deleted".to_owned());
    };
    let mode = match mode {
        "120000" => "120000",
        "100755" => "100755",
        _ => "100644",
    };
    let hash = match mode {
        "120000" => tools::git_raw(dir, &["cat-file", "blob", id])?.iter().map(|byte| format!("{byte:02x}")).collect(),
        _ => id.to_owned(),
    };
    Ok(format!("{mode} {hash}"))
}

#[cfg(test)]
mod tests {
    use super::fingerprint;

    #[test]
    fn a_path_below_a_file_or_link_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        std::fs::write(dir.join("src"), "a file where a folder was\n").unwrap();
        std::fs::create_dir_all(dir.join("elsewhere/inner")).unwrap();
        std::fs::write(dir.join("elsewhere/inner/lib.rs"), "fn x() {}\n").unwrap();
        std::os::unix::fs::symlink("elsewhere", dir.join("linked")).unwrap();
        assert_eq!(fingerprint(dir, "src/lib.rs").unwrap(), "deleted");
        assert_eq!(fingerprint(dir, "linked/inner/lib.rs").unwrap(), "deleted");
        assert_eq!(fingerprint(dir, "missing/lib.rs").unwrap(), "deleted");
        assert_eq!(fingerprint(dir, "elsewhere").unwrap(), "deleted");
        assert!(fingerprint(dir, "linked").unwrap().starts_with("120000 "));
    }
}
