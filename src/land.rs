//! `gft land`: a copy's changes become a commit on a branch on GitHub, and a pull
//! request, made through GitHub's API from the files themselves. No local commit or
//! push is involved, so the copy needs no history and git in it is never asked.
//!
//! The commit's tree is the copy's base commit's tree with the copy's changes laid
//! over it, so it is always the copy's whole state. Its parent is the branch's last
//! commit when there is one, so each landing adds a commit to the pull request; after
//! `gft sync` the new base becomes a second parent, so the pull request shows only the
//! copy's own changes.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};

use crate::github::GitHub;
use crate::state::{Pr, Store};
use crate::tools::{Change, changes, git, short};

pub fn land(
    store: &Store,
    repo: &str,
    name: &str,
    message: Option<&str>,
    body: Option<&str>,
    draft: bool,
) -> Result<()> {
    let project = store.load(repo)?;
    let copy = crate::copy(&project, name)?.clone();
    crate::mount_copy(&copy)?;
    let changed = changes(&copy.workspace, None, &copy.dir)?;
    if changed.is_empty() {
        bail!("{name} has no changes to land");
    }
    let given = body.is_some();
    let github = GitHub::new(repo)?;

    // The branch it lands on, and what that branch's pull request is based on: the
    // copy it came from, when that one has landed, else the default branch.
    let branch = copy.pr.as_ref().map_or_else(|| format!("graft/{name}"), |pr| pr.branch.clone());
    let from_pr = copy.from.as_ref().and_then(|from| project.copies.get(from)).and_then(|from| from.pr.clone());
    let pr_base = from_pr.as_ref().map_or(project.branch.clone(), |pr| pr.branch.clone());

    let base_tree = commit_tree(&github, &copy.base)?;
    let mut entries = Vec::new();
    let mut landed = BTreeMap::new();
    for change in &changed {
        // Before reading the file: if it changes in between, it counts as not landed.
        landed.insert(change.path.clone(), fingerprint(&copy.dir, change)?);
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
    let tree = github.call("POST", "git/trees", Some(&json!({"base_tree": base_tree, "tree": entries})))?;
    let tree = sha(&tree)?;

    let head =
        github.get_opt(&format!("git/ref/heads/{branch}"))?.map(|r| r["object"]["sha"].as_str().map(str::to_owned));
    let head = head.flatten();
    let stacked_on = match (&head, &from_pr) {
        (None, Some(pr)) => github
            .get_opt(&format!("git/ref/heads/{}", pr.branch))?
            .and_then(|r| r["object"]["sha"].as_str().map(str::to_owned)),
        _ => None,
    };
    let mut parents: Vec<String> = head.clone().or(stacked_on).into_iter().collect();
    if let Some(head) = &head {
        if commit_tree(&github, head)? == tree {
            bail!("nothing new to land: {branch} already has {name} as it is");
        }
        // Moved onto a newer commit since the last landing: that commit joins in.
        if copy.pr.as_ref().is_some_and(|pr| pr.landed_on != copy.base) {
            parents.push(copy.base.clone());
        }
    }
    if parents.is_empty() {
        parents.push(copy.base.clone());
    }
    let title = message.map_or_else(|| name.to_owned(), str::to_owned);
    let commit_message = match (message, &head) {
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
    match head {
        Some(_) => github.call("PATCH", &format!("git/refs/heads/{branch}"), Some(&json!({"sha": commit})))?,
        None => {
            github.call("POST", "git/refs", Some(&json!({"ref": format!("refs/heads/{branch}"), "sha": commit})))?
        }
    };

    // The description is kept in step with the one given, and always ends with where
    // the pull request came from.
    let footer = format!("Landed with `gft land` from the copy `{name}`.");
    let body = match body.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{text}\n\n{footer}"),
        None => footer,
    };
    let pr = match &copy.pr {
        Some(pr) => {
            if given {
                github.call("PATCH", &format!("pulls/{}", pr.number), Some(&json!({"body": body})))?;
            }
            Pr { landed_on: copy.base.clone(), landed, ..pr.clone() }
        }
        None => {
            let made = github.call(
                "POST",
                "pulls",
                Some(&json!({"title": title, "head": branch, "base": pr_base, "body": body, "draft": draft})),
            )?;
            Pr {
                number: made["number"].as_u64().context("GitHub gave no pull request number")?,
                url: made["html_url"].as_str().unwrap_or_default().to_owned(),
                branch: branch.clone(),
                landed_on: copy.base.clone(),
                landed,
            }
        }
    };
    let url = pr.url.clone();
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        copy.pr = Some(pr);
        Ok(())
    })?;
    println!("{branch} at {}, {} files: {url}", short(&commit), changed.len());
    Ok(())
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

/// What a changed file looked like, so `gft rm` can tell whether it is still what
/// landed. It is the file's own mode and git hash, not its modification time or the
/// copy's base: `gft sync` rewrites times and moves the base without touching what
/// the copy changed, and judging by either would call a landed copy unlanded (or,
/// the other way, miss an edit made inside the same second as the landing). A file
/// that `gft sync` merged upstream lines into reads as changed, which it is: landing
/// again would add a commit.
pub fn fingerprint(dir: &Path, change: &Change) -> Result<String> {
    if change.kind == 'D' {
        return Ok("deleted".to_owned());
    }
    let path = dir.join(&change.path);
    let (mode, link) = mode(&path)?;
    let hash = match link {
        Some(target) => String::from_utf8_lossy(&target).into_owned(),
        None => git(dir, &["hash-object", "--", &change.path])?,
    };
    Ok(format!("{mode} {hash}"))
}
