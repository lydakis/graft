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
}

pub fn land(store: &Store, repo: &str, name: &str, options: &Options) -> Result<()> {
    let project = store.load(repo)?;
    let mut copy = crate::copy(&project, name)?.clone();
    crate::mount_copy(&copy)?;
    let github = GitHub::new(repo)?;
    // The branch it lands on, and what that branch's pull request is based on: the
    // branch of the landing it was made on top of, else the default branch.
    let branch = copy.landed.as_ref().map_or_else(|| format!("graft/{name}"), |landed| landed.branch.clone());
    let pr_base = copy.stacked.as_ref().map_or(project.branch.clone(), |stacked| stacked.branch.clone());
    let head = github
        .get_opt(&format!("git/ref/heads/{branch}"))?
        .map(|found| found["object"]["sha"].as_str().map(str::to_owned).context("GitHub gave a branch without a sha"))
        .transpose()?;

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
    // The description is kept in step with the one given, and always ends with where
    // the pull request came from.
    let footer = format!("Landed with `gft land` from the copy `{name}`.");
    let given = options.body.is_some();
    let body = match options.body.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{text}\n\n{footer}"),
        None => footer,
    };
    let describe = |number: u64| github.call("PATCH", &format!("pulls/{number}"), Some(&json!({"body": body})));

    let base_tree = commit_tree(&github, &copy.base)?;
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
            // Only the description is new.
            if given {
                describe(pr.number)?;
                println!("{branch} already has {name} as it is; described it anew: {}", pr.url);
                return Ok(());
            }
            bail!("nothing new to land: {branch} already has {name} as it is");
        }
        Some(pr) => {
            if given {
                describe(pr.number)?;
            }
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

/// Change the copy `name` as kept on disk.
fn record(store: &Store, repo: &str, name: &str, change: impl FnOnce(&mut crate::state::Copy)) -> Result<()> {
    store.edit(repo, |project| {
        change(project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?);
        Ok(())
    })
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
