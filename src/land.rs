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
use crate::state::{PendingLand, Pr, Store};
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
    if let Some(pending) = &copy.pending_land {
        let github = GitHub::new(repo)?;
        let body = body.map(|body| description(name, Some(body)));
        let pr = finish_first(store, &github, repo, name, pending, body.as_deref())?;
        println!(
            "recovered {} at {}: {}; files left as they are; run `gft land` again for further edits",
            pr.branch,
            short(&pending.commit),
            pr.url
        );
        return Ok(());
    }
    crate::mount_copy(&copy)?;
    let changed = changes(&copy.workspace, None, &copy.dir)?;
    if changed.is_empty() {
        bail!("{name} has no changes to land");
    }
    let github = GitHub::new(repo)?;
    // The description is kept in step with the one given, and always ends with where
    // the pull request came from.
    let given = body.is_some();
    let body = description(name, body);
    let describe = |number: u64| github.call("PATCH", &format!("pulls/{number}"), Some(&json!({"body": body})));

    // The branch it lands on, and what that branch's pull request is based on: the
    // copy it came from, when that one has landed, else the default branch.
    let branch = copy.pr.as_ref().map_or_else(|| format!("graft/{name}"), |pr| pr.branch.clone());
    let from_pr = copy.from.as_ref().and_then(|from| project.copies.get(from)).and_then(|from| from.pr.clone());
    let pr_base = from_pr.as_ref().map_or(project.branch.clone(), |pr| pr.branch.clone());

    let head = github
        .get_opt(&format!("git/ref/heads/{branch}"))?
        .map(|r| r["object"]["sha"].as_str().map(str::to_owned).context("GitHub gave a branch without a sha"))
        .transpose()?;
    if copy.pr.is_none() && head.is_some() {
        bail!(
            "{branch} already exists, but {name} has no landing record; use a different copy name; no branch was changed"
        );
    }

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

    let stacked_on = match (&head, &from_pr) {
        (None, Some(pr)) => github
            .get_opt(&format!("git/ref/heads/{}", pr.branch))?
            .and_then(|r| r["object"]["sha"].as_str().map(str::to_owned)),
        _ => None,
    };
    let mut parents: Vec<String> = head.clone().or(stacked_on).into_iter().collect();
    if let Some(head) = &head {
        if commit_tree(&github, head)? == tree {
            // Only the description is new.
            if let (true, Some(pr)) = (given, &copy.pr) {
                describe(pr.number)?;
                println!("{branch} already has {name} as it is; described it anew: {}", pr.url);
                return Ok(());
            }
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
    let pr = match &copy.pr {
        Some(pr) => {
            match head {
                Some(_) => github.call("PATCH", &format!("git/refs/heads/{branch}"), Some(&json!({"sha": commit})))?,
                None => github.call(
                    "POST",
                    "git/refs",
                    Some(&json!({"ref": format!("refs/heads/{branch}"), "sha": commit})),
                )?,
            };
            if given {
                describe(pr.number)?;
            }
            Pr { landed_on: copy.base.clone(), landed, ..pr.clone() }
        }
        None => {
            let pending = PendingLand {
                commit: commit.clone(),
                branch: branch.clone(),
                landed_on: copy.base.clone(),
                landed,
                base: pr_base,
                title,
                body,
                draft,
            };
            // Save ownership and the exact files before making the branch visible.
            // A lost response must not make a retry guess from a branch name alone.
            store.edit(repo, |project| {
                let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
                if copy.pr.is_some() || copy.pending_land.is_some() {
                    bail!("{name}'s landing record changed; retry `gft land`");
                }
                copy.pending_land = Some(pending.clone());
                Ok(())
            })?;
            let pr = finish_first(store, &github, repo, name, &pending, None)?;
            println!("{branch} at {}, {} files: {}", short(&commit), changed.len(), pr.url);
            return Ok(());
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

fn description(name: &str, body: Option<&str>) -> String {
    let footer = format!("Landed with `gft land` from the copy `{name}`.");
    match body.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{text}\n\n{footer}"),
        None => footer,
    }
}

/// Finish only the saved landing: edits since that attempt stay unlanded. Neither
/// tree equality nor the copy name proves ownership of a branch or pull request.
fn finish_first(
    store: &Store,
    github: &GitHub,
    repo: &str,
    name: &str,
    pending: &PendingLand,
    body: Option<&str>,
) -> Result<Pr> {
    let branch = &pending.branch;
    let head = github.get_opt(&format!("git/ref/heads/{branch}"))?;
    if head.as_ref().is_some_and(|head| head["object"]["sha"].as_str() != Some(&pending.commit)) {
        bail!("{branch} moved since {name}'s interrupted landing; no branch or pull request was changed");
    }
    // Include closed PRs: a closed or merged request is not a missing request.
    let owner = repo.split_once('/').context("a GitHub repository needs owner/name")?.0;
    let head_filter = format!("{owner}%3A{}", branch.replace('/', "%2F"));
    let pulls = github.call("GET", &format!("pulls?state=all&head={head_filter}&per_page=100"), None)?;
    let pulls = pulls.as_array().context("GitHub gave no pull request list")?;
    let found = match pulls.as_slice() {
        [] => None,
        [pr] if matches_first_pr(pr, pending, repo) => Some(pr),
        _ => bail!(
            "{branch} has a closed or unrelated pull request, or more than one; no pull request was adopted or changed"
        ),
    };
    // A merged PR may have had its branch deleted. Do not resurrect it before
    // checking history, even though the saved commit still belongs to this copy.
    if head.is_none() {
        github.call(
            "POST",
            "git/refs",
            Some(&json!({"ref": format!("refs/heads/{branch}"), "sha": pending.commit})),
        )?;
    }
    let made = match found {
        Some(pr) => pr.clone(),
        None => github.call(
            "POST",
            "pulls",
            Some(&json!({
                "title": pending.title, "head": branch, "base": pending.base,
                "body": body.unwrap_or(&pending.body), "draft": pending.draft,
            })),
        )?,
    };
    // The branch can move between checking it and creating the PR. Do not mark
    // the saved files as landed unless the creation response confirms them too.
    if !matches_first_pr(&made, pending, repo) {
        bail!("{branch}'s pull request does not match the saved landing; its landing record was kept for recovery");
    }
    let pr = Pr {
        number: made["number"].as_u64().context("GitHub gave no pull request number")?,
        url: made["html_url"].as_str().context("GitHub gave no pull request URL")?.to_owned(),
        branch: pending.branch.clone(),
        landed_on: pending.landed_on.clone(),
        landed: pending.landed.clone(),
    };
    if let (Some(_), Some(body)) = (found, body) {
        github.call("PATCH", &format!("pulls/{}", pr.number), Some(&json!({"body": body})))?;
    }
    store.edit(repo, |project| {
        let copy = project.copies.get_mut(name).with_context(|| format!("no copy {name:?}"))?;
        if copy.pending_land.as_ref().map(|p| &p.commit) != Some(&pending.commit) || copy.pr.is_some() {
            bail!("{name}'s landing record changed; retry `gft land`");
        }
        copy.pr = Some(pr.clone());
        copy.pending_land = None;
        Ok(())
    })?;
    Ok(pr)
}

fn matches_first_pr(pr: &Value, pending: &PendingLand, repo: &str) -> bool {
    pr["state"] == "open"
        && pr["head"]["repo"]["full_name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(repo))
        && pr["head"]["ref"] == pending.branch
        && pr["head"]["sha"] == pending.commit
        && pr["base"]["repo"]["full_name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(repo))
        && pr["base"]["ref"] == pending.base
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
/// landed. It is the file's own mode and the git hash of its bytes as landing sends
/// them, without the repository's filters, not its modification time or the
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
        // In hex: a link's target is any bytes, and two differing ones must not read the same.
        Some(target) => target.iter().map(|byte| format!("{byte:02x}")).collect(),
        None => git(dir, &["hash-object", "--no-filters", "--", &change.path])?,
    };
    Ok(format!("{mode} {hash}"))
}
