//! `gft land`: a copy's changes become a commit on a branch `graft/NAME` on the
//! project's remote, pushed with plain git, and, where the remote has a forge Graft
//! knows (GitHub, GitLab), a pull or merge request. The commit is made from the files
//! themselves over the base commit's tree, so the copy needs no history, and git in
//! it (its index, branch, commits) is left as it was.
//!
//! The commit's tree is the copy's base commit's tree with the copy's changes laid
//! over it, so it is always the copy's whole state. Its first parent is the commit
//! the copy's last landing left the branch at; on a first landing, the landing the
//! copy was made on top of, if any, else its base. After `gft sync` the new base
//! becomes a second parent, so the review shows only the copy's own changes.
//!
//! Each step is recorded as soon as it is done, so a landing cut short (a lost
//! connection, the forge refusing the review request) is finished by landing again.
//! A branch that moved on the remote in a way the copy can't account for is left
//! alone: the copy doesn't have those commits' changes, and landing over them would
//! undo them. Pushes carry a lease to that effect, so a branch that moves while a
//! landing is under way is left alone too.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::forge::{Forge, Review, State};
use crate::remote::{self, Remote};
use crate::state::{Landing, Pr, Project, Store};
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
    crate::refuse_conflicts(&marked, options.allow_conflicts)?;
    let forge = project.forge.as_ref().map(Forge::new);
    let remote = Remote::of(&project);
    let noun = forge.as_ref().map_or("pull request", Forge::noun);
    // Without a token the push would go through and the review request wouldn't.
    if let Some(forge) = &forge
        && !options.dry_run
    {
        forge.signed_in()?;
    }
    // Built on work merged since: its files still have that work as it was, and sync
    // takes in the merged version.
    if let Some(stacked) = &copy.stacked
        && let Some(merged) = merged_into(&project, &remote, forge.as_ref(), &copy.dir, stacked)?
    {
        bail!(
            "{} was merged ({merged}); `gft sync {name}` to move onto {} with it, then land",
            stacked.branch,
            project.branch
        );
    }
    // The review request as the forge has it now. One merged, or closed, takes no
    // more commits: the copy lands afresh, on a branch and a review of its own.
    let mut open_base = None;
    if let (Some(pr), Some(forge)) = (copy.pr.clone(), &forge) {
        let review = forge.get(pr.number)?;
        let open = review.state == State::Open;
        if open && !options.new_pr {
            open_base = Some(review.base.clone());
        } else {
            let merged = review.state == State::Merged;
            if merged && review.base != project.branch {
                let into = &review.base;
                bail!(
                    "{noun} #{} was merged into {into}, not {}: once {into} is merged, `gft sync {name}` moves \
                     {name} onto {} with both, and it lands again as a new {noun}",
                    pr.number,
                    project.branch,
                    project.branch
                );
            }
            if merged {
                if let Some(merge) = &review.merge
                    && !forge.contains(&copy.base, merge)?
                {
                    bail!(
                        "{noun} #{} was merged; `gft sync {name}` first, so what lands next goes on top of it",
                        pr.number
                    );
                }
                println!("{noun} #{} was merged; landing {name} as a new one", pr.number);
            } else if !open && !options.new_pr {
                bail!(
                    "{noun} #{} was closed without being merged. Reopen it on {} and land again, or land with \
                     --new-pr to open another",
                    pr.number,
                    forge.name()
                );
            } else {
                println!("leaving {noun} #{} as it is; landing {name} as a new one", pr.number);
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
    // Every branch the copy's name could take, in one call to the remote.
    let heads = remote.heads(&copy.dir, &format!("graft/{name}*"))?;
    let mut head = heads.get(&branch).cloned();

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
            head = heads.get(&branch).cloned();
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
            "{branch} on {} {state}; landing now would undo what they changed. Take their changes into \
             {name} first, or land with --force to replace them",
            forge.as_ref().map_or("the remote", Forge::name)
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
    if options.dry_run {
        let pr = copy.pr.as_ref().filter(|_| head.is_some());
        return preview(name, &branch, &pr_base, head.as_deref(), &parents, pr, &changed, ours.is_none(), noun);
    }
    // The description is kept in step with the one given, and always ends with where
    // the review request came from.
    let footer = format!("Landed with `gft land` from the copy `{name}`.");
    let given = options.body.is_some();
    let body = match options.body.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{text}\n\n{footer}"),
        None => footer,
    };
    // A review request that goes against another branch than it should, as one built
    // on work merged since, goes against the right one.
    let retarget = |forge: &Forge, number: u64| -> Result<bool> {
        match &open_base {
            Some(base) if *base != pr_base => {
                forge.retarget(number, &pr_base)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    };

    // The commits it builds on, which the copy may not have: each is fetched alone, so
    // the push doesn't go looking through a history the copy lacks.
    let others: Vec<&str> = parents.iter().map(String::as_str).filter(|parent| *parent != copy.base).collect();
    let ours_commit = ours.map(|ours| ours.commit.as_str());
    remote.fetch_commits(&copy.dir, &others.iter().copied().chain(ours_commit).collect::<Vec<_>>())?;
    let mut files = BTreeMap::new();
    for change in &changed {
        // Before reading the file: if it changes in between, it counts as not landed.
        files.insert(change.path.clone(), fingerprint(&copy.dir, &change.path)?);
    }
    let commit_message = match (options.message, &head) {
        (Some(message), _) => message.to_owned(),
        (None, None) => format!("Changes from {name}"),
        (None, Some(_)) => format!("Update {name}"),
    };
    let (made, tree) = remote::commit(&copy.dir, &copy.base, &changed, &parents, &commit_message)?;
    // How many of the files differ from what the last landing carried.
    let fresh = match ours {
        Some(ours) => {
            files.iter().filter(|(path, now)| ours.files.get(*path) != Some(now)).count()
                + ours.files.keys().filter(|path| !files.contains_key(*path)).count()
        }
        None => files.len(),
    };
    // The branch holds the copy as it is already, as after a landing whose review
    // request was never made.
    let current = match ours {
        Some(ours) => remote::tree_of(&copy.dir, &ours.commit)? == tree,
        None => false,
    };
    // A branch gone from the remote (deleted once its review was merged or closed)
    // is made again, with a review of its own.
    let pr = copy.pr.clone().filter(|_| head.is_some());

    let mut links = Vec::new();
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
            // Recorded before the branch moves, so a landing cut short from here on
            // can tell its own commit on the branch from someone else's.
            let landing = Landing { branch: branch.clone(), commit: made.clone(), on: copy.base.clone(), files };
            record(store, repo, name, |copy| copy.pending = Some(landing.clone()))?;
            links = remote.push(&copy.dir, &made, &branch, head.as_deref())?;
            record(store, repo, name, |copy| {
                copy.pending = None;
                copy.landed = Some(landing);
                copy.pr = pr.clone();
            })?;
            made
        }
    };
    let fresh = match ours {
        Some(_) if !current && fresh != changed.len() => format!(", {fresh} changed since the last landing"),
        _ => String::new(),
    };
    let Some(forge) = &forge else {
        if current {
            bail!("nothing new to land: {branch} already has {name} as it is");
        }
        println!("{branch} at {}, {}{fresh}", short(&commit), crate::files(changed.len()));
        for link in links {
            println!("{link}");
        }
        return Ok(());
    };
    let describe = |number: u64| forge.describe(number, &body);

    let pr = match pr {
        Some(pr) if current => {
            // Only the description, or what the review goes against, is new.
            let moved = retarget(forge, pr.number)?;
            if given {
                describe(pr.number)?;
                println!("{branch} already has {name} as it is; described it anew: {}", pr.url);
                return Ok(());
            }
            if moved {
                println!("{branch} already has {name} as it is; its {noun} now goes against {pr_base}: {}", pr.url);
                return Ok(());
            }
            bail!("nothing new to land: {branch} already has {name} as it is");
        }
        Some(pr) => {
            if given {
                describe(pr.number)?;
            }
            retarget(forge, pr.number)?;
            pr
        }
        // One made by a landing whose answer never came back is found, not made twice.
        None => match forge.open_from(&branch)? {
            Some(review) => {
                if given {
                    describe(review.number)?;
                }
                as_pr(&review)
            }
            None => {
                let title = options.message.map_or_else(|| name.to_owned(), str::to_owned);
                as_pr(&forge.create(&title, &branch, &pr_base, &body, options.draft)?)
            }
        },
    };
    let url = pr.url.clone();
    record(store, repo, name, |copy| copy.pr = Some(pr))?;
    println!("{branch} at {}, {}{fresh}: {url}", short(&commit), crate::files(changed.len()));
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
    noun: &str,
) -> Result<()> {
    let moves = match head {
        None => "made new".to_owned(),
        Some(head) if replaces => format!("moved from {}, replacing commits {name} didn't land", short(head)),
        Some(head) => format!("moved from {}", short(head)),
    };
    let parents: Vec<&str> = parents.iter().map(|parent| short(parent)).collect();
    println!("branch        {branch} ({moves})");
    println!("commit        on {}", parents.join(" and "));
    let label = format!("{noun:<13}");
    match pr {
        Some(pr) => println!("{label} {} (updated)", pr.url),
        None => println!("{label} new or open one, against {pr_base}"),
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

/// Delete the branch the copy last landed on, once its review request is merged or
/// closed and the branch holds nothing but what the copy landed. Says what happened.
pub fn delete_branch(project: &Project, copy: &crate::state::Copy) -> Result<String> {
    let Some(landed) = &copy.landed else { return Ok("never landed, so no branch".to_owned()) };
    let branch = &landed.branch;
    let remote = Remote::of(project);
    if let Some(forge) = project.forge.as_ref().map(Forge::new) {
        // A landing cut off before it heard back may have opened one it didn't record.
        let open = match &copy.pr {
            Some(pr) => (forge.get(pr.number)?.state == State::Open).then_some(pr.number),
            None => forge.open_from(branch)?.map(|review| review.number),
        };
        if let Some(number) = open {
            return Ok(format!("kept branch {branch}: {} #{number} is still open", forge.noun()));
        }
    }
    let kept = format!("kept branch {branch}: it has commits this copy didn't land");
    match remote.head(&copy.dir, branch)? {
        None => Ok(format!("branch {branch} is gone already")),
        Some(head) if head != landed.commit => Ok(kept),
        Some(_) if !remote.delete(&copy.dir, branch, &landed.commit)? => Ok(kept),
        Some(_) => Ok(format!("deleted branch {branch}")),
    }
}

/// Whether `landing` is in the default branch, and how it got there: the review
/// request that carried it, or, on a remote without a forge Graft knows, the
/// default branch having every file as the landing left it.
pub fn merged_into(
    project: &Project,
    remote: &Remote,
    forge: Option<&Forge>,
    dir: &Path,
    landing: &Landing,
) -> Result<Option<String>> {
    if landing.commit.is_empty() {
        return Ok(None);
    }
    if let Some(forge) = forge {
        return Ok(merged_pull(forge, landing, &project.branch)?.map(|number| format!("#{number}")));
    }
    if landing.files.is_empty() {
        return Ok(None);
    }
    let Some(tip) = remote.head(dir, &project.branch)? else { return Ok(None) };
    remote.fetch_commits(dir, &[&tip])?;
    for (path, landed) in &landing.files {
        if fingerprint_at(dir, &tip, path)? != *landed {
            return Ok(None);
        }
    }
    Ok(Some(format!("{} has its files", project.branch)))
}

/// The number of the review request that carried `landing` into the branch `into`
/// (the default branch), if one did: one from its branch whose last commit has the
/// landing's. A branch name used again after an earlier review from it was merged
/// doesn't count. One merged into another copy's branch counts once that branch's own
/// review, with the merge, reaches `into`, however many branches the work went
/// through on the way.
pub fn merged_pull(forge: &Forge, landing: &Landing, into: &str) -> Result<Option<u64>> {
    let (mut branch, mut commit) = (landing.branch.clone(), landing.commit.clone());
    let mut first = None;
    // A stack is a few copies deep; the bound only stops a cycle of branches.
    for _ in 0..16 {
        let mut next = None;
        for review in forge.merged_from(&branch)? {
            if !review.head.is_empty() && forge.contains(&review.head, &commit)? {
                next = Some(review);
                break;
            }
        }
        let Some(review) = next else { return Ok(None) };
        let number = *first.get_or_insert(review.number);
        if review.base == into {
            return Ok(Some(number));
        }
        // Merged into another branch: its work reaches `into` with that branch's.
        let Some(merge) = review.merge else { return Ok(None) };
        (branch, commit) = (review.base, merge);
    }
    Ok(None)
}

/// The bytes of `path` in `commit`, none when the commit has no file there, read in
/// the copy at `dir`, which fetches the commit if it lacks it.
pub fn file_at(remote: &Remote, dir: &Path, commit: &str, path: &str) -> Result<Option<Vec<u8>>> {
    remote.fetch_commits(dir, &[commit])?;
    let listed = git(dir, &["ls-tree", "-z", commit, "--", path])?;
    let Some((about, _)) = listed.trim_end_matches('\0').split_once('\t') else { return Ok(None) };
    match about.split(' ').collect::<Vec<_>>()[..] {
        [mode, "blob", id] if mode != "120000" => Ok(Some(tools::git_raw(dir, &["cat-file", "blob", id])?)),
        _ => Ok(None),
    }
}

fn as_pr(review: &Review) -> Pr {
    Pr::new(review.number, review.url.clone())
}

/// The git mode of the file or link at `path`, and a link's target.
pub fn mode(path: &Path) -> Result<(&'static str, Option<Vec<u8>>)> {
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
