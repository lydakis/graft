//! `gft` end to end: copies of a repository served from a folder, mounted by sandfs,
//! landing on that repository with git push, and on a small stand-in for GitHub's
//! API for pull requests.
//!
//! Needs a working mount: on Linux FUSE 3, on macOS nothing. Skipped without one,
//! unless `GRAFT_REQUIRE_MOUNT` is set, as CI does.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

fn mounts() -> bool {
    let mounts = cfg!(target_os = "macos")
        || (Path::new("/dev/fuse").exists()
            && Command::new("fusermount3").arg("--version").output().is_ok_and(|out| out.status.success()));
    if !mounts {
        assert!(std::env::var_os("GRAFT_REQUIRE_MOUNT").is_none(), "a mount is required but can't be made here");
        eprintln!("skipping: no mount");
    }
    mounts
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// The branches of the repository at `repo`, by name, with the commit each is at.
fn branches(repo: &Path) -> BTreeMap<String, String> {
    git(repo, &["for-each-ref", "--format=%(refname:strip=2) %(objectname)", "refs/heads"])
        .lines()
        .filter_map(|line| line.split_once(' ').map(|(name, commit)| (name.to_owned(), commit.to_owned())))
        .collect()
}

/// The commit the branch `branch` of the repository at `repo` is at.
fn branch(repo: &Path, branch: &str) -> String {
    branches(repo).remove(branch).unwrap_or_else(|| panic!("no branch {branch}"))
}

/// The parents of `commit`.
fn parents(repo: &Path, commit: &str) -> Vec<String> {
    git(repo, &["rev-list", "--parents", "-n1", commit]).split(' ').skip(1).map(str::to_owned).collect()
}

/// The files `commit` changed from its first parent.
fn changed_by(repo: &Path, commit: &str) -> Vec<String> {
    let listed = git(repo, &["diff-tree", "--no-commit-id", "--name-only", "-r", &format!("{commit}^1"), commit]);
    listed.lines().map(str::to_owned).collect()
}

/// The text of `path` in `commit`, if it has that file.
fn file_at(repo: &Path, commit: &str, path: &str) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(["show", &format!("{commit}:{path}")]).output().unwrap();
    out.status.success().then(|| String::from_utf8(out.stdout).unwrap())
}

/// Make a commit on the branch `name` of the repository at `repo` with `path`
/// holding `text`, as someone else pushing there would.
fn push_there(repo: &Path, name: &str, path: &str, text: &str) -> String {
    let at = branches(repo).get(name).cloned().unwrap_or_else(|| git(repo, &["rev-parse", "main"]));
    let index = repo.join(".git/other-index");
    let run = |args: &[&str], input: Option<&str>| {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["-c", "user.name=o", "-c", "user.email=o@o"])
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.unwrap_or_default().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    run(&["read-tree", &at], None);
    let blob = run(&["hash-object", "-w", "--stdin"], Some(text));
    run(&["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{path}")], None);
    let tree = run(&["write-tree"], None);
    let commit = run(&["commit-tree", &tree, "-p", &at], Some("someone else's\n"));
    git(repo, &["update-ref", &format!("refs/heads/{name}"), &commit]);
    let _ = std::fs::remove_file(index);
    commit
}

/// Have the next push to the repository at `repo` fail: before the branch moves, or
/// (`after`) once it has, as when the answer is lost on the way back.
fn cut_next_push(repo: &Path, after: bool) {
    let hook = repo.join(".git/hooks/update");
    std::fs::write(
        &hook,
        "#!/bin/sh\n\
         if [ -e cut-after ]; then rm cut-after; git update-ref \"$1\" \"$3\"; exit 1; fi\n\
         if [ -e cut-before ]; then rm cut-before; exit 1; fi\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join(".git").join(if after { "cut-after" } else { "cut-before" }), "").unwrap();
}

/// What the stand-in GitHub was asked to make.
#[derive(Default)]
struct Made {
    pulls: Vec<Value>,
    /// The bodies of PATCH pulls/N, as `(N, body)`.
    patches: Vec<(u64, Value)>,
    /// Fail the next pull request without making it.
    fail_pull: bool,
    /// Make the next pull request but answer with a failure.
    lose_pull: bool,
    /// The repository copies land on, which the pull requests' branches are in.
    repo: PathBuf,
    /// Pull requests moved to another base, as `(N, base)`.
    retargets: Vec<(u64, String)>,
}

/// A pull request as GitHub answers for it. Its last commit is its branch's, until
/// it is merged or closed.
fn pull_json(made: &Made, n: usize) -> Value {
    let pull = &made.pulls[n - 1];
    let head = pull
        .get("head_sha")
        .cloned()
        .unwrap_or_else(|| json!(branches(&made.repo).get(pull["head"].as_str().unwrap())));
    json!({"number": n, "html_url": format!("https://github.com/o/n/pull/{n}"),
        "state": pull.get("state").cloned().unwrap_or(json!("open")),
        "merged_at": pull.get("merged_at").cloned().unwrap_or(Value::Null),
        "merge_commit_sha": pull.get("merge_commit_sha").cloned().unwrap_or(Value::Null),
        "head": {"ref": pull["head"], "sha": head},
        "base": {"ref": pull["base"]}})
}

/// Close pull request `n` as merged by the commit `merge`, keeping its last commit.
fn merge_pull(made: &mut Made, n: usize, merge: &str) {
    let head = pull_json(made, n)["head"]["sha"].clone();
    let pull = &mut made.pulls[n - 1];
    pull["state"] = json!("closed");
    pull["merged_at"] = json!("2026-10-26T00:00:00Z");
    pull["merge_commit_sha"] = json!(merge);
    pull["head_sha"] = head;
}

/// Answers the calls `gft land` and `gft sync` make about pull requests, keeping what
/// they were sent, for the repository at `repo`.
fn fake_github(repo: &Path) -> (String, Arc<Mutex<Made>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let made = Arc::new(Mutex::new(Made { repo: repo.to_owned(), ..Made::default() }));
    let state = made.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut parts = line.split_whitespace();
            let (method, path) = (parts.next().unwrap().to_owned(), parts.next().unwrap().to_owned());
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap() };
            let mut made = state.lock().unwrap();
            if let Some((_, path)) = path.split_once("/projects/o%2Fn/") {
                let (status, answer) = gitlab(&mut made, &method, path, body);
                drop(made);
                let answer = answer.to_string();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                    answer.len()
                );
                continue;
            }
            let path = path.split_once("/repos/o/n").unwrap().1.trim_start_matches('/').to_owned();
            let (status, answer) = match (method.as_str(), path.as_str()) {
                ("GET", p) if p.starts_with("pulls?head=o:") => {
                    let mut query = p["pulls?head=o:".len()..].split('&');
                    let branch = query.next().unwrap();
                    let state = query.find_map(|part| part.strip_prefix("state=")).unwrap_or("open");
                    let found: Vec<Value> = (1..=made.pulls.len())
                        .filter(|n| made.pulls[n - 1]["head"] == branch)
                        .map(|n| pull_json(&made, n))
                        .filter(|pull| state == "all" || pull["state"] == state)
                        .collect();
                    (200, json!(found))
                }
                ("GET", p) if p.starts_with("pulls/") => {
                    let n: usize = p["pulls/".len()..].parse().unwrap();
                    (200, pull_json(&made, n))
                }
                ("GET", p) if p.starts_with("compare/") => {
                    let (from, to) = p["compare/".len()..].split_once("...").unwrap();
                    let ancestor = Command::new("git")
                        .arg("-C")
                        .arg(&made.repo)
                        .args(["merge-base", "--is-ancestor", from, to])
                        .stderr(Stdio::null())
                        .status()
                        .unwrap();
                    let status = match (from == to, ancestor.success(), ancestor.code()) {
                        (true, _, _) => "identical",
                        (false, true, _) => "ahead",
                        (false, false, Some(1)) => "diverged",
                        _ => "unknown",
                    };
                    match status {
                        "unknown" => (404, json!({"message": "Not Found"})),
                        status => (200, json!({"status": status})),
                    }
                }
                ("POST", "pulls") if std::mem::take(&mut made.fail_pull) => (502, json!({"message": "cut short"})),
                ("POST", "pulls") if !branches(&made.repo).contains_key(body["head"].as_str().unwrap()) => {
                    (422, json!({"message": "Validation Failed: no such head branch"}))
                }
                ("POST", "pulls") => {
                    made.pulls.push(body);
                    let n = made.pulls.len();
                    match std::mem::take(&mut made.lose_pull) {
                        true => (502, json!({"message": "cut short"})),
                        false => (201, json!({"number": n, "html_url": format!("https://github.com/o/n/pull/{n}")})),
                    }
                }
                ("PATCH", p) if p.starts_with("pulls/") => {
                    let number: u64 = p["pulls/".len()..].parse().unwrap();
                    if let Some(base) = body["base"].as_str() {
                        made.pulls[number as usize - 1]["base"] = json!(base);
                        made.retargets.push((number, base.to_owned()));
                    }
                    if !body["body"].is_null() {
                        made.patches.push((number, body["body"].clone()));
                    }
                    (200, pull_json(&made, number as usize))
                }
                _ => (404, json!({"message": format!("no {method} {path}")})),
            };
            drop(made);
            let answer = answer.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
        }
    });
    (url, made)
}

/// A merge request as GitLab answers for it, from a pull request kept as GitHub's.
fn merge_request_json(made: &Made, n: usize) -> Value {
    let pull = pull_json(made, n);
    let state = match (pull["state"].as_str(), pull["merged_at"].is_string()) {
        (_, true) => "merged",
        (Some("open"), _) => "opened",
        _ => "closed",
    };
    json!({"iid": n, "web_url": format!("https://gitlab.com/o/n/-/merge_requests/{n}"), "state": state,
        "source_branch": pull["head"]["ref"], "target_branch": pull["base"]["ref"], "sha": pull["head"]["sha"],
        "merge_commit_sha": pull["merge_commit_sha"], "title": made.pulls[n - 1]["title"],
        "description": made.pulls[n - 1]["body"]})
}

/// Answers GitLab's calls about merge requests, kept with GitHub's pull requests.
fn gitlab(made: &mut Made, method: &str, path: &str, body: Value) -> (u16, Value) {
    let query = |name: &str| path.split(['?', '&']).find_map(|part| part.strip_prefix(&format!("{name}=")));
    match method {
        "GET" if path.starts_with("merge_requests?") => {
            let (branch, state) = (query("source_branch").unwrap(), query("state").unwrap());
            let found: Vec<Value> = (1..=made.pulls.len())
                .filter(|n| made.pulls[n - 1]["head"] == branch)
                .map(|n| merge_request_json(made, n))
                .filter(|request| request["state"] == state)
                .collect();
            (200, json!(found))
        }
        "GET" if path.starts_with("merge_requests/") => {
            (200, merge_request_json(made, path["merge_requests/".len()..].parse().unwrap()))
        }
        "POST" if path == "merge_requests" => {
            made.pulls.push(json!({"head": body["source_branch"], "base": body["target_branch"],
                "title": body["title"], "body": body["description"]}));
            let n = made.pulls.len();
            (201, merge_request_json(made, n))
        }
        "PUT" if path.starts_with("merge_requests/") => {
            let n: usize = path["merge_requests/".len()..].parse().unwrap();
            if let Some(base) = body["target_branch"].as_str() {
                made.pulls[n - 1]["base"] = json!(base);
                made.retargets.push((n as u64, base.to_owned()));
            }
            if !body["description"].is_null() {
                made.patches.push((n as u64, body["description"].clone()));
            }
            (200, merge_request_json(made, n))
        }
        "GET" if path.starts_with("repository/merge_base?") => {
            let refs: Vec<&str> = path.split(['?', '&']).filter_map(|part| part.strip_prefix("refs[]=")).collect();
            let out = Command::new("git").arg("-C").arg(&made.repo).arg("merge-base").args(&refs).output().unwrap();
            match out.status.success() {
                true => (200, json!({"id": String::from_utf8_lossy(&out.stdout).trim()})),
                false => (404, json!({"message": "404 Not Found"})),
            }
        }
        _ => (404, json!({"message": format!("no {method} {path}")})),
    }
}

struct Gft {
    sandfs_home: tempfile::TempDir,
    graft_home: tempfile::TempDir,
    api: String,
}

impl Gft {
    fn try_run(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_gft"))
            .current_dir(dir)
            .env("SANDFS_HOME", self.sandfs_home.path())
            .env("GRAFT_HOME", self.graft_home.path())
            .env("GRAFT_GITHUB_API", &self.api)
            .env("GRAFT_GITLAB_API", &self.api)
            .env("GITHUB_TOKEN", "test-token")
            .env("GITLAB_TOKEN", "test-token")
            .args(args)
            .output()
            .unwrap()
    }

    /// Like `run`, with `input` on standard input.
    fn run_with_input(&self, dir: &Path, args: &[&str], input: &str) -> String {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gft"))
            .current_dir(dir)
            .env("SANDFS_HOME", self.sandfs_home.path())
            .env("GRAFT_HOME", self.graft_home.path())
            .env("GRAFT_GITHUB_API", &self.api)
            .env("GRAFT_GITLAB_API", &self.api)
            .env("GITHUB_TOKEN", "test-token")
            .env("GITLAB_TOKEN", "test-token")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "gft {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn run(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.try_run(dir, args);
        assert!(out.status.success(), "gft {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Gft {
    fn drop(&mut self) {
        let Ok(home) = sandfs::Home::at(self.sandfs_home.path()) else { return };
        for workspace in home.workspaces().unwrap_or_default() {
            if workspace.mounted.is_some() {
                let _ = home.unmount(&workspace.name);
            }
        }
    }
}

#[test]
fn copies_are_made_compared_synced_and_landed() {
    if !mounts() {
        return;
    }
    // Served over file://, so sandfs treats it as a remote repository.
    let upstream = tempfile::Builder::new().prefix("graft-upstream").tempdir().unwrap();
    let repo = upstream.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::create_dir(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/lib.rs"), "fn one() {}\n\nfn two() {}\n").unwrap();
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/target\n").unwrap();
    // A file kept in an ignored folder all the same.
    std::fs::create_dir(repo.join("target")).unwrap();
    std::fs::write(repo.join("target/keep"), "kept\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["add", "-f", "target/keep"]);
    git(repo, &["commit", "-q", "-m", "first"]);
    let first = git(repo, &["rev-parse", "HEAD"]);

    let (api, made) = fake_github(repo);
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    let url = format!("file://{}", repo.display());
    let got = gft.run(&work, &["get", &url, "app", "--forge", "github:o/n"]);
    assert!(got.contains("o/n at") && got.contains("(main)"), "{got}");
    let home = work.join("app");
    assert_eq!(std::fs::read_to_string(home.join("README.md")).unwrap(), "hello\n");

    // Copies sit next to the home copy, on the newest commit.
    let made_copies = gft.run(&home, &["new", "fix", "try"]);
    let (fix, try_) = (work.join("app+fix"), work.join("app+try"));
    assert_eq!(made_copies, format!("{}\n{}\n", fix.display(), try_.display()));
    std::fs::write(fix.join("src/lib.rs"), "fn one() {}\n\nfn two() { 2 }\n").unwrap();
    std::fs::write(fix.join("notes.txt"), "why\n").unwrap();
    std::fs::write(try_.join("src/lib.rs"), "fn one() { 1 }\n\nfn two() {}\n").unwrap();
    // Build output git ignores is no change, in either copy or between them.
    for (copy, built) in [(&fix, "built\n"), (&try_, "built another way\n")] {
        std::fs::create_dir_all(copy.join("target/debug")).unwrap();
        std::fs::write(copy.join("target/debug/app"), built).unwrap();
    }
    assert_eq!(gft.run(&fix, &["changes"]), "A notes.txt\nM src/lib.rs\n");
    assert_eq!(gft.run(&fix, &["changes", "fix", "try"]), "A notes.txt\nM src/lib.rs\n");
    assert_eq!(gft.run(&fix, &["changes", "--count"]), "1 added, 1 modified, 0 deleted\n");
    assert_eq!(gft.run(&fix, &["changes", "fix", "try", "--count"]), "1 added, 1 modified, 0 deleted\n");
    let listed = gft.run(&home, &["ls"]);
    assert!(listed.contains("fix\t2 files\tbase ") && listed.contains("overlaps try on src/lib.rs"), "{listed}");
    // Outside any copy, `ls` lists every project; inside one, just that project.
    assert_eq!(gft.run(&work, &["ls"]), listed);
    assert_eq!(gft.run(&work, &["ls"]).matches("(home, main)").count(), 1);
    assert_eq!(gft.run(&home, &["path", "fix"]), format!("{}\n", fix.display()));
    // A folder replaced by a link: git isn't asked about what was in it, past the link.
    gft.run(&home, &["new", "link"]);
    let link = work.join("app+link");
    std::fs::remove_dir_all(link.join("src")).unwrap();
    std::os::unix::fs::symlink("elsewhere", link.join("src")).unwrap();
    assert_eq!(gft.run(&link, &["changes"]), "A src\nD src/lib.rs\n");
    // A base file deleted with `git rm` is a change, though git ignores its name now.
    git(&link, &["rm", "-q", "target/keep"]);
    assert_eq!(gft.run(&link, &["changes"]), "A src\nD src/lib.rs\nD target/keep\n");
    gft.run(&home, &["rm", "link", "--force"]);

    // A snapshot, a wrong turn, and back.
    gft.run(&fix, &["save", "two returns 2"]);
    let status = gft.run(&fix, &["status"]);
    assert!(status.contains("copy       fix\n") && status.contains("changed    2 files\n"), "{status}");
    assert!(status.contains("snapshots  1\n") && !status.contains("pull request"), "{status}");
    assert!(gft.run(&home, &["status"]).contains("copies   2\n"));
    std::fs::remove_file(fix.join("notes.txt")).unwrap();
    std::fs::write(fix.join("stray.txt"), "oops\n").unwrap();
    // From inside the copy, with a program working there all along.
    let mut shell = Command::new("sleep").arg("30").current_dir(&fix).spawn().unwrap();
    gft.run(&fix, &["undo"]);
    assert_eq!(std::fs::read_to_string(fix.join("notes.txt")).unwrap(), "why\n");
    assert!(!fix.join("stray.txt").exists());
    shell.kill().unwrap();
    shell.wait().unwrap();
    gft.run(&fix, &["save", "again"]);
    // Undo kept the wrong turn too, as a snapshot of its own.
    let listed = gft.run(&home, &["undo", "--list", "--copy", "fix"]);
    let lines: Vec<&str> = listed.lines().collect();
    assert!(lines.len() == 3 && lines[0].starts_with("1\t") && lines[2].starts_with("3\t"), "{listed}");
    assert!(lines[0].ends_with(" sec ago\tagain") && lines[1].ends_with(" sec ago\tbefore undo"), "{listed}");
    assert!(lines[2].ends_with(" sec ago\ttwo returns 2"), "{listed}");
    gft.run(&home, &["undo", "--copy", "fix"]);

    // Upstream changes another line of the same file: sync merges it in.
    std::fs::write(repo.join("src/lib.rs"), "// lib\nfn one() {}\n\nfn two() {}\n").unwrap();
    std::fs::write(repo.join("CHANGES.md"), "- one\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "second"]);
    let second = git(repo, &["rev-parse", "HEAD"]);
    // From inside the copy, with a program working in one of its folders.
    let mut shell = Command::new("sleep").arg("30").current_dir(fix.join("src")).spawn().unwrap();
    let synced = gft.run(&fix, &["sync"]);
    assert!(synced.contains("merged\tsrc/lib.rs"), "{synced}");
    assert!(shell.try_wait().unwrap().is_none());
    shell.kill().unwrap();
    shell.wait().unwrap();
    assert_eq!(std::fs::read_to_string(fix.join("src/lib.rs")).unwrap(), "// lib\nfn one() {}\n\nfn two() { 2 }\n");
    assert_eq!(std::fs::read_to_string(fix.join("CHANGES.md")).unwrap(), "- one\n");
    assert_eq!(gft.run(&fix, &["changes"]), "A notes.txt\nM src/lib.rs\n");
    assert_ne!(first, second);

    // Landing makes a branch, one commit on the newest base, and a pull request.
    let landed = gft.run(&fix, &["land", "-m", "Make two return 2", "--body", "Two now returns 2.\n"]);
    let one = branch(repo, "graft/fix");
    assert!(
        landed.contains(&format!("graft/fix at {}, 2 files: https://github.com/o/n/pull/1", &one[..10])),
        "{landed}"
    );
    assert!(gft.run(&fix, &["status"]).contains("pull request  https://github.com/o/n/pull/1\n"));
    {
        let made = made.lock().unwrap();
        assert_eq!(changed_by(repo, &one), ["notes.txt", "src/lib.rs"]);
        assert_eq!(file_at(repo, &one, "src/lib.rs").unwrap(), "// lib\nfn one() {}\n\nfn two() { 2 }\n");
        assert_eq!(file_at(repo, &one, "CHANGES.md").unwrap(), "- one\n");
        assert_eq!(parents(repo, &one), std::slice::from_ref(&second));
        assert_eq!(git(repo, &["log", "-1", "--format=%s", &one]), "Make two return 2");
        // Git in the copy is left as it was: no commit, nothing staged.
        assert_eq!(git(&fix, &["rev-parse", "HEAD"]), second);
        assert!(git(&fix, &["diff", "--cached", "--name-only"]).is_empty());
        assert_eq!(made.pulls[0]["base"], "main");
        assert_eq!(made.pulls[0]["title"], "Make two return 2");
        assert_eq!(made.pulls[0]["body"], "Two now returns 2.\n\nLanded with `gft land` from the copy `fix`.");
    }
    // Landing again adds a commit to the same pull request.
    std::fs::write(fix.join("notes.txt"), "why, and how\n").unwrap();
    let again = gft.run(&fix, &["land"]);
    assert!(again.contains("pull/1"), "{again}");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.pulls.len(), 1);
        let two = branch(repo, "graft/fix");
        assert_eq!(parents(repo, &two), std::slice::from_ref(&one));
        assert_eq!(git(repo, &["log", "-1", "--format=%s", &two]), "Update fix");
        // No body given: the description on GitHub is left alone.
        assert!(made.patches.is_empty());
    }
    // A body given on a later landing updates the pull request, from a file or stdin.
    std::fs::write(fix.join("notes.txt"), "why, how, and when\n").unwrap();
    let described = work.join("description.md");
    std::fs::write(&described, "From a file.\n").unwrap();
    gft.run(&fix, &["land", "--body-file", described.to_str().unwrap()]);
    std::fs::write(fix.join("notes.txt"), "why, how, when, and who\n").unwrap();
    gft.run_with_input(&fix, &["land", "--body-file", "-"], "From stdin.\n\nWith two paragraphs.\n");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.pulls.len(), 1);
        let footer = "\n\nLanded with `gft land` from the copy `fix`.";
        assert_eq!(made.patches[0], (1, json!(format!("From a file.{footer}"))));
        assert_eq!(made.patches[1], (1, json!(format!("From stdin.\n\nWith two paragraphs.{footer}"))));
    }
    let both = gft.try_run(&fix, &["land", "--body", "a", "--body-file", "-"]);
    assert!(!both.status.success());
    let unchanged = gft.try_run(&fix, &["land"]);
    assert!(String::from_utf8_lossy(&unchanged.stderr).contains("nothing new to land"));
    // With nothing new but a description, only the description changes.
    let before = branch(repo, "graft/fix");
    let redescribed = gft.run(&fix, &["land", "--body", "Only words."]);
    assert!(redescribed.contains("described it anew"), "{redescribed}");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.patches[2], (1, json!("Only words.\n\nLanded with `gft land` from the copy `fix`.")));
        assert_eq!(branch(repo, "graft/fix"), before, "no new commit");
    }

    // A copy of a copy starts with its changes and lands on top of its branch.
    gft.run(&home, &["new", "more", "--from", "fix"]);
    let more = work.join("app+more");
    assert_eq!(std::fs::read_to_string(more.join("notes.txt")).unwrap(), "why, how, when, and who\n");

    // Copy names that are missing, taken or invalid are refused plainly.
    let missing = String::from_utf8_lossy(&gft.try_run(&home, &["path", "nope"]).stderr).into_owned();
    assert!(missing.contains("has no copy \"nope\"; its copies are: ") && missing.contains("fix"), "{missing}");
    let taken = String::from_utf8_lossy(&gft.try_run(&home, &["new", "fix"]).stderr).into_owned();
    assert!(taken.contains("already has a copy \"fix\"; pick another name"), "{taken}");
    let bad = String::from_utf8_lossy(&gft.try_run(&home, &["new", "two words"]).stderr).into_owned();
    assert!(bad.contains("invalid copy name \"two words\""), "{bad}");

    // Changes that never landed are not thrown away, unless asked: `try` was never
    // landed, `more` carries `fix`'s changes without a pull request of its own.
    let kept = gft.try_run(&home, &["rm", "try", "more"]);
    assert!(!kept.status.success());
    let said = String::from_utf8_lossy(&kept.stderr);
    assert!(said.contains("try has 1 file that never landed") && said.contains("more has 2 files"), "{said}");
    assert!(gft.run(&home, &["ls"]).contains("try"));
    // A landed copy goes without --force, even after a sync moved its base; an edit
    // since the landing, or one reverted, is what counts.
    std::fs::write(repo.join("CHANGES.md"), "- one\n- two\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "third"]);
    gft.run(&fix, &["sync"]);
    std::fs::write(fix.join("notes.txt"), "edited after landing\n").unwrap();
    let edited = gft.try_run(&home, &["rm", "fix"]);
    assert!(String::from_utf8_lossy(&edited.stderr).contains("fix has 1 file that never landed"));
    std::fs::write(fix.join("notes.txt"), "why, how, when, and who\n").unwrap();
    gft.run(&home, &["rm", "fix"]);
    gft.run(&home, &["rm", "--force", "try", "more"]);
    assert!(!gft.run(&home, &["ls"]).contains("try"));
}

#[test]
fn copies_of_a_plain_folder_land_back_into_it() {
    if !mounts() {
        return;
    }
    let (api, _) = fake_github(Path::new("/nowhere"));
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    let folder = work.join("notes");
    std::fs::create_dir_all(folder.join("drafts")).unwrap();
    std::fs::write(folder.join("todo.md"), "- a\n").unwrap();
    std::fs::write(folder.join("drafts/one.md"), "one\n").unwrap();

    let got = gft.run(&work, &["get", "./notes"]);
    assert!(got.contains("a plain folder"), "{got}");
    assert!(gft.run(&work, &["ls"]).contains("notes\t") && gft.run(&work, &["ls"]).contains("(folder)"));
    // A copy is the folder as it is when made, which needs a disk that clones files.
    let made = gft.try_run(&folder, &["new", "edit", "other"]);
    if String::from_utf8_lossy(&made.stderr).contains("clones files") {
        eprintln!("skipping: this disk doesn't clone files (try TMPDIR on APFS, btrfs or XFS)");
        return;
    }
    assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
    let (edit, other) = (work.join("notes+edit"), work.join("notes+other"));
    std::fs::write(folder.join("later.md"), "made in the folder later\n").unwrap();
    assert!(!edit.join("later.md").exists(), "the folder's later changes stay out");
    std::fs::write(edit.join("todo.md"), "- a\n- b\n").unwrap();
    std::fs::remove_file(edit.join("drafts/one.md")).unwrap();
    assert_eq!(gft.run(&edit, &["changes"]), "D drafts/one.md\nM todo.md\n");
    assert!(gft.run(&edit, &["status"]).contains("copied     "));
    let refused = gft.try_run(&edit, &["land", "-m", "a title"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("are for pull requests"));
    assert!(String::from_utf8_lossy(&gft.try_run(&folder, &["changes"]).stderr).contains("name a copy"));

    // Synced from inside, with a program working there: the copy takes in what the
    // folder has now and keeps its own changes.
    let mut shell = Command::new("sleep").arg("30").current_dir(edit.join("drafts")).spawn().unwrap();
    let synced = gft.run(&edit, &["sync"]);
    assert!(synced.contains("0 files changed on both sides"), "{synced}");
    assert_eq!(std::fs::read_to_string(edit.join("later.md")).unwrap(), "made in the folder later\n");
    assert_eq!(gft.run(&edit, &["changes"]), "D drafts/one.md\nM todo.md\n");

    // Landing writes the changes into the folder; then they are the folder's own.
    let landed = gft.run(&edit, &["land"]);
    assert!(landed.contains("2 files written to"), "{landed}");
    assert_eq!(std::fs::read_to_string(folder.join("todo.md")).unwrap(), "- a\n- b\n");
    assert!(!folder.join("drafts/one.md").exists());
    assert_eq!(gft.run(&edit, &["changes"]), "");
    shell.kill().unwrap();
    shell.wait().unwrap();
    gft.run(&folder, &["rm", "edit"]);

    // A copy that changed a file the folder has changed since lands nothing, and
    // says how to go on in Graft's terms.
    std::fs::write(other.join("todo.md"), "- z\n- a\n").unwrap();
    std::fs::write(other.join("new.md"), "new\n").unwrap();
    let clash = gft.try_run(&other, &["land"]);
    assert!(!clash.status.success());
    let said = String::from_utf8_lossy(&clash.stderr);
    assert!(said.contains("todo.md") && said.contains("`gft sync other`") && !said.contains("--force"), "{said}");
    assert!(!folder.join("new.md").exists(), "nothing was written");
    // A sync merges the folder's change in, from the folder as the copy took it, so
    // landing keeps both.
    let synced = gft.run(&other, &["sync"]);
    assert!(synced.contains("merged\ttodo.md") && synced.contains("0 need a look"), "{synced}");
    gft.run(&other, &["land"]);
    assert_eq!(std::fs::read_to_string(folder.join("todo.md")).unwrap(), "- z\n- a\n- b\n");
    assert_eq!(std::fs::read_to_string(folder.join("new.md")).unwrap(), "new\n");
    gft.run(&folder, &["rm", "other"]);
    assert!(!gft.run(&folder, &["ls"]).contains("other"));

    // A file the copy made a link, changed in the folder too, isn't merged through
    // the link: what it points at stays as it was.
    gft.run(&folder, &["new", "linked"]);
    let linked = folder.parent().unwrap().join("notes+linked");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("target.md"), "outside\n").unwrap();
    std::fs::remove_file(linked.join("todo.md")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("target.md"), linked.join("todo.md")).unwrap();
    std::fs::write(folder.join("todo.md"), "- z\n- a\n- b\n- c\n").unwrap();
    let synced = gft.run(&linked, &["sync"]);
    assert!(synced.contains("kept yours\ttodo.md"), "{synced}");
    assert_eq!(std::fs::read_to_string(outside.path().join("target.md")).unwrap(), "outside\n");
    gft.run(&folder, &["rm", "--force", "linked"]);
    // Nor is a link the folder had read through for the bytes it once held.
    std::os::unix::fs::symlink("/dev/zero", folder.join("zero")).unwrap();
    gft.run(&folder, &["new", "zeroed"]);
    let zeroed = folder.parent().unwrap().join("notes+zeroed");
    std::fs::remove_file(zeroed.join("zero")).unwrap();
    assert!(gft.run(&zeroed, &["sync"]).contains("0 files changed on both sides"));
    gft.run(&folder, &["rm", "--force", "zeroed"]);
    std::fs::remove_file(folder.join("zero")).unwrap();

    // A folder inside another one can be a project of its own; inside it, it wins
    // (named to come after the outer one, which is looked at first).
    let inner = folder.join("zone");
    std::fs::create_dir(&inner).unwrap();
    std::fs::write(inner.join("only.md"), "inner\n").unwrap();
    gft.run(&folder, &["get", "./zone"]);
    gft.run(&inner, &["new", "sub"]);
    assert_eq!(std::fs::read_to_string(folder.join("zone+sub/only.md")).unwrap(), "inner\n");
    gft.run(&inner, &["rm", "sub"]);
}

#[test]
fn landings_cut_short_are_finished_and_never_undo_others_work() {
    if !mounts() {
        return;
    }
    let upstream = tempfile::Builder::new().prefix("graft-upstream").tempdir().unwrap();
    let repo = upstream.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "first"]);
    let first = git(repo, &["rev-parse", "HEAD"]);
    let (api, made) = fake_github(repo);
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    gft.run(&work, &["get", &format!("file://{}", repo.display()), "app", "--forge", "github:o/n"]);
    let home = work.join("app");
    gft.run(&home, &["new", "fix", "two", "three", "four", "taken"]);
    let at = |name: &str| work.join(format!("app+{name}"));
    let fails = |name: &str, args: &[&str]| {
        let out = gft.try_run(&at(name), args);
        assert!(!out.status.success(), "gft {args:?} in {name} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    for name in ["fix", "two", "three", "four", "taken"] {
        std::fs::write(at(name).join("notes.txt"), format!("{name}\n")).unwrap();
    }

    // A dry run says what landing would do, and does none of it.
    let plan = gft.run(&at("fix"), &["land", "--dry-run"]);
    assert_eq!(
        plan,
        format!(
            "branch        graft/fix (made new)\ncommit        on {}\npull request  new or open one, against main\n\
             files         1 file\nA notes.txt\n",
            &first[..10]
        )
    );
    assert_eq!(branches(repo).len(), 1, "only main");

    // The branch moves but the answer is lost: landing again sees its own commit
    // there and goes on to the pull request, without a second commit.
    cut_next_push(repo, true);
    fails("fix", &["land", "-m", "Fix"]);
    let one = branch(repo, "graft/fix");
    let landed = gft.run(&at("fix"), &["land", "-m", "Fix"]);
    assert!(
        landed.contains(&format!("graft/fix at {}, 1 file: https://github.com/o/n/pull/1", &one[..10])),
        "{landed}"
    );
    {
        let made = made.lock().unwrap();
        assert_eq!(branch(repo, "graft/fix"), one);
        assert_eq!(made.pulls.len(), 1);
        assert_eq!(made.pulls[0]["title"], "Fix");
    }
    // The commit is made but the branch never moves to it: landing again starts afresh.
    cut_next_push(repo, false);
    fails("two", &["land"]);
    assert!(!branches(repo).contains_key("graft/two"));
    gft.run(&at("two"), &["land"]);
    assert_eq!(parents(repo, &branch(repo, "graft/two")), std::slice::from_ref(&first));
    // GitHub makes the pull request but its answer is lost, or refuses to make it:
    // landing again finds the one made, or makes it, and never makes two.
    made.lock().unwrap().lose_pull = true;
    fails("three", &["land"]);
    gft.run(&at("three"), &["land"]);
    made.lock().unwrap().fail_pull = true;
    fails("four", &["land"]);
    let landed = gft.run(&at("four"), &["land"]);
    assert!(landed.contains("pull/4"), "{landed}");
    {
        let made = made.lock().unwrap();
        let heads: Vec<&Value> = made.pulls.iter().map(|pull| &pull["head"]).collect();
        assert_eq!(heads, [&json!("graft/fix"), &json!("graft/two"), &json!("graft/three"), &json!("graft/four")]);
        // One commit each.
        for name in ["three", "four"] {
            assert_eq!(parents(repo, &branch(repo, &format!("graft/{name}"))), std::slice::from_ref(&first));
        }
    }
    // A branch of that name that this copy never landed is someone else's, even with
    // the same files, or an earlier copy's of the same name: a first landing takes a
    // name of its own and none of that branch's history.
    let theirs = push_there(repo, "graft/taken", "notes.txt", "taken\n");
    let landed = gft.run(&at("taken"), &["land"]);
    assert!(landed.contains("graft/taken-2 at "), "{landed}");
    assert_eq!(branch(repo, "graft/taken"), theirs);
    assert_eq!(parents(repo, &branch(repo, "graft/taken-2")), std::slice::from_ref(&first));
    gft.run(&home, &["rm", "taken"]);

    // Someone adds a commit to fix's branch: landing over it would undo it.
    let pushed = push_there(repo, "graft/fix", "theirs.txt", "theirs\n");
    std::fs::write(at("fix").join("notes.txt"), "fix, and more\n").unwrap();
    let said = fails("fix", &["land"]);
    assert!(said.contains("graft/fix on GitHub has moved since fix last landed"), "{said}");
    // Even put back as its base has it, fix lands over them when told to.
    std::fs::remove_file(at("fix").join("notes.txt")).unwrap();
    let said = fails("fix", &["land"]);
    assert!(said.contains("has moved since"), "{said}");
    gft.run(&at("fix"), &["land", "--force"]);
    let forced = branch(repo, "graft/fix");
    assert_eq!(parents(repo, &forced), [pushed]);
    assert_eq!(
        git(repo, &["rev-parse", &format!("{forced}^{{tree}}")]),
        git(repo, &["rev-parse", &format!("{first}^{{tree}}")])
    );
    // And someone adds to the branch again.
    let pushed = push_there(repo, "graft/fix", "theirs.txt", "theirs, again\n");
    std::fs::write(at("fix").join("notes.txt"), "fix, and more\n").unwrap();
    let plan = gft.run(&at("fix"), &["land", "--force", "--dry-run"]);
    assert!(plan.contains(&format!("(moved from {}, replacing commits fix didn't land)", &pushed[..10])), "{plan}");
    assert_eq!(branch(repo, "graft/fix"), pushed);
    gft.run(&at("fix"), &["land", "--force"]);
    let forced = branch(repo, "graft/fix");
    assert_eq!(parents(repo, &forced), [pushed]);

    // Only a copy that landed can be built on.
    gft.run(&home, &["new", "unlanded"]);
    let said = String::from_utf8_lossy(&gft.try_run(&home, &["new", "nope", "--on", "unlanded"]).stderr).into_owned();
    assert!(said.contains("unlanded hasn't landed, so there is no branch to build on"), "{said}");
    gft.run(&home, &["rm", "unlanded"]);

    // A copy built on fix sits on fix's landing as it was: fix landing more since
    // changes neither what it holds nor what its own landing follows.
    gft.run(&home, &["new", "child", "--on", "fix"]);
    // Another attempt from fix's files is no stack: it goes to main with all of them.
    gft.run(&home, &["new", "alt", "--from", "fix"]);
    let plan = gft.run(&at("alt"), &["land", "-n"]);
    assert!(plan.contains("against main\n") && plan.contains(&format!("on {}\n", &first[..10])), "{plan}");
    gft.run(&home, &["rm", "--force", "alt"]);
    std::fs::write(at("fix").join("notes.txt"), "fix, more, and later\n").unwrap();
    gft.run(&at("fix"), &["land"]);
    assert_ne!(branch(repo, "graft/fix"), forced);
    std::fs::write(at("child").join("child.txt"), "on top\n").unwrap();
    let plan = gft.run(&at("child"), &["land", "-n"]);
    assert!(plan.contains(&format!("commit        on {}\n", &forced[..10])), "{plan}");
    assert!(plan.contains("against graft/fix\n"), "{plan}");
    gft.run(&at("child"), &["land"]);
    {
        let made = made.lock().unwrap();
        let child = branch(repo, "graft/child");
        assert_eq!(parents(repo, &child), std::slice::from_ref(&forced));
        assert_eq!(file_at(repo, &child, "notes.txt").unwrap(), "fix, and more\n");
        let pull = made.pulls.last().unwrap();
        assert_eq!((&pull["head"], &pull["base"]), (&json!("graft/child"), &json!("graft/fix")));
    }
    // Synced onto a newer commit, it lands with that commit as a second parent.
    std::fs::write(repo.join("README.md"), "hello again\n").unwrap();
    git(repo, &["commit", "-q", "-am", "second"]);
    let second = git(repo, &["rev-parse", "HEAD"]);
    gft.run(&at("child"), &["sync"]);
    std::fs::write(at("child").join("child.txt"), "on top, synced\n").unwrap();
    let before = branch(repo, "graft/child");
    gft.run(&at("child"), &["land"]);
    assert_eq!(parents(repo, &branch(repo, "graft/child")), [before, second]);

    // fix put back as its base has it: still a change, which `rm` keeps until landed.
    std::fs::remove_file(at("fix").join("notes.txt")).unwrap();
    assert_eq!(gft.run(&at("fix"), &["changes"]), "");
    let said = String::from_utf8_lossy(&gft.try_run(&home, &["rm", "fix"]).stderr).into_owned();
    assert!(said.contains("fix has 1 file that never landed"), "{said}");
    let landed = gft.run(&at("fix"), &["land"]);
    assert!(landed.contains("0 files"), "{landed}");
    let fix = branch(repo, "graft/fix");
    assert_eq!(
        git(repo, &["rev-parse", &format!("{fix}^{{tree}}")]),
        git(repo, &["rev-parse", &format!("{first}^{{tree}}")])
    );
    gft.run(&home, &["rm", "fix"]);

    // Merged upstream, then changed there again: synced, nothing is lost by `rm`.
    gft.run(&home, &["new", "merged"]);
    std::fs::write(at("merged").join("merged.txt"), "landed\n").unwrap();
    gft.run(&at("merged"), &["land"]);
    std::fs::write(repo.join("merged.txt"), "landed\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "merge"]);
    gft.run(&at("merged"), &["sync"]);
    std::fs::write(repo.join("merged.txt"), "changed upstream since\n").unwrap();
    git(repo, &["commit", "-q", "-am", "later"]);
    gft.run(&at("merged"), &["sync"]);
    assert_eq!(gft.run(&at("merged"), &["changes"]), "");
    gft.run(&home, &["rm", "merged"]);
}

#[test]
fn copies_of_a_checkout_start_from_its_work() {
    if !mounts() {
        return;
    }
    let upstream = tempfile::Builder::new().prefix("graft-upstream").tempdir().unwrap();
    let repo = upstream.path();
    git(repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "first"]);
    let (api, made) = fake_github(repo);
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    git(&work, &["clone", "-q", &format!("file://{}", repo.display()), "app"]);
    let app = work.join("app");
    std::fs::write(app.join("README.md"), "hello, edited\n").unwrap();
    std::fs::write(app.join("new.txt"), "new\n").unwrap();
    let head = git(&app, &["rev-parse", "HEAD"]);

    // On a branch of its own, with no word from origin on its default branch:
    // origin says which it is.
    git(&app, &["remote", "set-head", "origin", "--delete"]);
    git(&app, &["checkout", "-q", "-b", "feature"]);
    gft.run(&work, &["get", "./app", "--forge", "github:o/n"]);
    assert!(gft.run(&app, &["ls"]).contains("(checkout, main)"));

    // A copy has the checkout's uncommitted work; the checkout keeps it too.
    gft.run(&app, &["new", "try"]);
    let copy = work.join("app+try");
    assert_eq!(std::fs::read_to_string(copy.join("README.md")).unwrap(), "hello, edited\n");
    assert_eq!(gft.run(&copy, &["changes"]), "M README.md\nA new.txt\n");
    assert_eq!(std::fs::read_to_string(app.join("README.md")).unwrap(), "hello, edited\n");
    std::fs::write(copy.join("more.txt"), "more\n").unwrap();
    gft.run(&copy, &["land"]);
    let landed = branch(repo, "graft/try");
    assert_eq!(parents(repo, &landed), [head]);
    assert_eq!(file_at(repo, &landed, "more.txt").unwrap(), "more\n");
    assert_eq!(made.lock().unwrap().pulls.len(), 1);

    // Committed in the checkout, the edit is the base's once the copy syncs.
    git(&app, &["commit", "-q", "-am", "edit"]);
    gft.run(&copy, &["sync"]);
    assert_eq!(gft.run(&copy, &["changes"]), "A more.txt\nA new.txt\n");
    gft.run(&app, &["rm", "--force", "try"]);
}

/// A repository on a stand-in GitHub, with a home copy at `work/app`.
fn project() -> (tempfile::TempDir, tempfile::TempDir, Gft, Arc<Mutex<Made>>) {
    let upstream = tempfile::Builder::new().prefix("graft-upstream").tempdir().unwrap();
    let repo = upstream.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(repo.join("README.md"), "one\ntwo\nthree\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "first"]);
    let (api, made) = fake_github(repo);
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    gft.run(work.path(), &["get", &format!("file://{}", repo.display()), "app", "--forge", "github:o/n"]);
    (upstream, work, gft, made)
}

fn commit_upstream(repo: &Path, path: &str, text: &str) -> String {
    std::fs::write(repo.join(path), text).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", path]);
    git(repo, &["rev-parse", "HEAD"])
}

#[test]
fn conflicts_hold_landing_until_settled() {
    if !mounts() {
        return;
    }
    let (upstream, work, gft, made) = project();
    let work = work.path().canonicalize().unwrap();
    let (home, copy) = (work.join("app"), work.join("app+fix"));
    gft.run(&home, &["new", "fix"]);
    std::fs::write(copy.join("README.md"), "one\nTWO here\nthree\n").unwrap();
    commit_upstream(upstream.path(), "README.md", "one\ntwo upstream\nthree\n");

    // A sync that leaves conflict markers says so, and exits with 2.
    let synced = gft.try_run(&copy, &["sync"]);
    assert_eq!(synced.status.code(), Some(2));
    let said = String::from_utf8_lossy(&synced.stdout);
    assert!(said.contains("conflict\tREADME.md") && said.contains("1 need a look"), "{said}");
    assert!(String::from_utf8_lossy(&synced.stderr).contains("1 file has conflict markers"));
    assert!(gft.run(&copy, &["status"]).contains("conflicts  README.md"));
    // Synced again, they are still there and still said.
    let again = gft.try_run(&copy, &["sync"]);
    assert_eq!(again.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&again.stdout).contains("conflict markers in: README.md"));
    // Landing refuses them, unless told to publish them as they are.
    let refused = gft.try_run(&copy, &["land"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("1 file still has conflict markers"));
    assert!(!branches(upstream.path()).contains_key("graft/fix"));
    let plan = gft.run(&copy, &["land", "-n", "--allow-conflicts"]);
    assert!(plan.contains("M README.md"), "{plan}");
    // Settled, it lands.
    gft.run(&copy, &["save", "with markers"]);
    std::fs::write(copy.join("README.md"), "one\nTWO upstream\nthree\n").unwrap();
    assert!(!gft.run(&copy, &["status"]).contains("conflicts"));
    gft.run(&copy, &["land"]);
    assert_eq!(made.lock().unwrap().pulls.len(), 1);
    let landed = branch(upstream.path(), "graft/fix");
    assert_eq!(file_at(upstream.path(), &landed, "README.md").unwrap(), "one\nTWO upstream\nthree\n");
    gft.run(&copy, &["sync"]);
    // Undone back to before they were settled, the markers count again.
    gft.run(&copy, &["undo"]);
    assert!(gft.run(&copy, &["status"]).contains("conflicts  README.md"));
    let refused = gft.try_run(&copy, &["land"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("still has conflict markers"));
}

#[test]
fn closed_and_merged_pull_requests_take_no_more_commits() {
    if !mounts() {
        return;
    }
    let (upstream, work, gft, made) = project();
    let repo = upstream.path();
    let work = work.path().canonicalize().unwrap();
    let home = work.join("app");
    let at = |name: &str| work.join(format!("app+{name}"));
    gft.run(&home, &["new", "closed", "merged"]);
    for name in ["closed", "merged"] {
        std::fs::write(at(name).join(format!("{name}.txt")), "first\n").unwrap();
        gft.run(&at(name), &["land"]);
    }

    // Closed without being merged: landing more needs reopening it, or a new one.
    made.lock().unwrap().pulls[0]["state"] = json!("closed");
    std::fs::write(at("closed").join("closed.txt"), "second\n").unwrap();
    let refused = gft.try_run(&at("closed"), &["land"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("#1 was closed without being merged"));
    assert_eq!(parents(repo, &branch(repo, "graft/closed")).len(), 1, "no second commit");
    let landed = gft.run(&at("closed"), &["land", "--new-pr", "--force"]);
    assert!(landed.contains("graft/closed-2 at ") && landed.contains("pull/3"), "{landed}");

    // Merged: what lands next needs to go on top of it, after a sync.
    let merge = commit_upstream(repo, "merged.txt", "first\n");
    merge_pull(&mut made.lock().unwrap(), 2, &merge);
    std::fs::write(at("merged").join("merged.txt"), "first\nand more\n").unwrap();
    let refused = gft.try_run(&at("merged"), &["land"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("#2 was merged; `gft sync merged` first"));
    // Synced, it takes in the merge without a conflict: what it landed is what
    // upstream's version is merged from.
    let synced = gft.run(&at("merged"), &["sync"]);
    assert!(synced.contains("merged\tmerged.txt") && synced.contains("#2 was merged"), "{synced}");
    assert_eq!(gft.run(&at("merged"), &["changes"]), "M merged.txt\n");
    let landed = gft.run(&at("merged"), &["land"]);
    assert!(landed.contains("graft/merged-2 at ") && landed.contains("pull/4"), "{landed}");
    assert_eq!(parents(repo, &branch(repo, "graft/merged-2")), [merge]);

    // `rm --delete-branch` deletes a branch whose pull request is done with, and
    // keeps one still open.
    made.lock().unwrap().pulls[2]["state"] = json!("closed");
    let removed = gft.run(&home, &["rm", "--delete-branch", "closed", "merged"]);
    assert!(removed.contains("closed: deleted branch graft/closed-2"), "{removed}");
    assert!(removed.contains("merged: kept branch graft/merged-2: pull request #4 is still open"), "{removed}");
    // A landing cut off before it heard of its pull request still keeps that branch.
    gft.run(&home, &["new", "lost"]);
    std::fs::write(at("lost").join("lost.txt"), "lost\n").unwrap();
    made.lock().unwrap().lose_pull = true;
    assert!(!gft.try_run(&at("lost"), &["land"]).status.success());
    let removed = gft.run(&home, &["rm", "--delete-branch", "lost"]);
    assert!(removed.contains("lost: kept branch graft/lost: pull request #5 is still open"), "{removed}");
    let left = branches(repo);
    assert!(!left.contains_key("graft/closed-2"));
    assert!(left.contains_key("graft/merged-2") && left.contains_key("graft/lost"));
}

#[test]
fn copies_built_on_merged_work_move_onto_main_with_it() {
    if !mounts() {
        return;
    }
    let (upstream, work, gft, made) = project();
    let repo = upstream.path();
    let work = work.path().canonicalize().unwrap();
    let home = work.join("app");
    let (parent, child) = (work.join("app+parent"), work.join("app+child"));
    gft.run(&home, &["new", "parent"]);
    std::fs::write(parent.join("README.md"), "one\ntwo from parent\nthree\n").unwrap();
    std::fs::write(parent.join("lib.txt"), "a\nb\n").unwrap();
    gft.run(&parent, &["land"]);
    gft.run(&home, &["new", "child", "--on", "parent"]);
    std::fs::write(child.join("lib.txt"), "a\nb\nc from child\n").unwrap();
    gft.run(&child, &["land"]);

    // The parent is squash-merged and its branch deleted, which closes the child's
    // pull request on GitHub.
    std::fs::write(repo.join("README.md"), "one\ntwo from parent\nthree\n").unwrap();
    let squash = commit_upstream(repo, "lib.txt", "a\nb\n");
    {
        let mut made = made.lock().unwrap();
        merge_pull(&mut made, 1, &squash);
        git(repo, &["branch", "-D", "graft/parent"]);
        let head = pull_json(&made, 2)["head"]["sha"].clone();
        made.pulls[1]["head_sha"] = head;
        made.pulls[1]["state"] = json!("closed");
    }
    // Landing first is sent to sync; sync merges from the parent's landing, so only
    // the child's own change is left, with no conflict.
    let refused = gft.try_run(&child, &["land"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("graft/parent was merged"));
    let synced = gft.run(&child, &["sync"]);
    assert!(synced.contains("now builds on main") && synced.contains("opens a new one"), "{synced}");
    assert!(!synced.contains("conflict"), "{synced}");
    assert_eq!(std::fs::read_to_string(child.join("lib.txt")).unwrap(), "a\nb\nc from child\n");
    assert_eq!(gft.run(&child, &["changes"]), "M lib.txt\n");
    let landed = gft.run(&child, &["land"]);
    assert!(landed.contains("graft/child-2 at ") && landed.contains("pull/3"), "{landed}");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.pulls[2]["base"], "main");
        assert_eq!(parents(repo, &branch(repo, "graft/child-2")), [squash]);
    }

    // A pull request still open when the work under it merges goes against main
    // on the next landing.
    gft.run(&home, &["new", "base2"]);
    std::fs::write(work.join("app+base2/two.txt"), "x\n").unwrap();
    gft.run(&work.join("app+base2"), &["land"]);
    gft.run(&home, &["new", "top", "--on", "base2"]);
    std::fs::write(work.join("app+top/top.txt"), "y\n").unwrap();
    gft.run(&work.join("app+top"), &["land"]);
    let merged = commit_upstream(repo, "two.txt", "x\n");
    merge_pull(&mut made.lock().unwrap(), 4, &merged);
    gft.run(&work.join("app+top"), &["sync"]);
    std::fs::write(work.join("app+top/top.txt"), "y\nz\n").unwrap();
    gft.run(&work.join("app+top"), &["land"]);
    assert_eq!(made.lock().unwrap().retargets, [(5, "main".to_owned())]);

    // Merged into the branch it was built on, which is still open: the copy stays
    // on that branch until it merges.
    gft.run(&home, &["new", "base3"]);
    std::fs::write(work.join("app+base3/three.txt"), "x\n").unwrap();
    gft.run(&work.join("app+base3"), &["land"]);
    gft.run(&home, &["new", "mid", "--on", "base3"]);
    std::fs::write(work.join("app+mid/mid.txt"), "y\n").unwrap();
    gft.run(&work.join("app+mid"), &["land"]);
    gft.run(&home, &["new", "leaf", "--on", "mid"]);
    // A link that lands as a link: nothing reads where it points.
    std::os::unix::fs::symlink("/dev/zero", work.join("app+leaf/zero")).unwrap();
    gft.run(&work.join("app+leaf"), &["land", "--dry-run"]);
    {
        let mut made = made.lock().unwrap();
        assert_eq!(made.pulls[6]["base"], "graft/base3");
        let head = pull_json(&made, 7)["head"]["sha"].clone();
        made.pulls[6]["head_sha"] = head;
        made.pulls[6]["state"] = json!("closed");
        made.pulls[6]["merged_at"] = json!("2026-10-26T00:00:00Z");
    }
    commit_upstream(repo, "elsewhere.txt", "z\n");
    let synced = gft.run(&work.join("app+mid"), &["sync"]);
    assert!(!synced.contains("new one") && !synced.contains("builds on main"), "{synced}");
    let refused = gft.try_run(&work.join("app+mid"), &["land"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("#7 was merged into graft/base3, not main"));
    // Nor is the copy built on it moved onto main, which has none of their work yet.
    let synced = gft.run(&work.join("app+leaf"), &["sync"]);
    assert!(!synced.contains("builds on main"), "{synced}");
    // Once the branch it was merged into reaches main, it moves onto main.
    std::fs::write(repo.join("three.txt"), "x\n").unwrap();
    let merged = commit_upstream(repo, "mid.txt", "y\n");
    {
        let mut made = made.lock().unwrap();
        let head = branch(repo, "graft/base3");
        made.pulls[6]["merge_commit_sha"] = json!(head);
        merge_pull(&mut made, 6, &merged);
    }
    let synced = gft.run(&work.join("app+leaf"), &["sync"]);
    assert!(synced.contains("graft/mid was merged (#7); leaf now builds on main"), "{synced}");
    // The copy merged on the way merges upstream's later edits from what it landed.
    commit_upstream(repo, "mid.txt", "y\nupstream\n");
    let synced = gft.run(&work.join("app+mid"), &["sync"]);
    assert!(synced.contains("#7 was merged; mid lands next as a new one"), "{synced}");
    assert!(!synced.contains("conflict"), "{synced}");
    assert_eq!(std::fs::read_to_string(work.join("app+mid/mid.txt")).unwrap(), "y\nupstream\n");
}

#[test]
fn undo_keeps_what_it_undoes() {
    if !mounts() {
        return;
    }
    let (_upstream, work, gft, _made) = project();
    let work = work.path().canonicalize().unwrap();
    let copy = work.join("app+try");
    gft.run(&work.join("app"), &["new", "try"]);
    std::fs::write(copy.join("README.md"), "saved\n").unwrap();
    gft.run(&copy, &["save", "good"]);
    std::fs::write(copy.join("README.md"), "unsaved\n").unwrap();
    let undone = gft.run(&copy, &["undo"]);
    assert!(undone.contains("`gft undo` again returns to how it was"), "{undone}");
    assert_eq!(std::fs::read_to_string(copy.join("README.md")).unwrap(), "saved\n");
    gft.run(&copy, &["undo"]);
    assert_eq!(std::fs::read_to_string(copy.join("README.md")).unwrap(), "unsaved\n");
    // Nothing changed since the last snapshot: no new one.
    let listed = gft.run(&copy, &["undo", "--list"]);
    assert_eq!(listed.lines().count(), 3, "{listed}");
    // A sync with nothing to take in keeps no snapshot either.
    gft.run(&copy, &["save", "now"]);
    std::fs::write(copy.join("README.md"), "unsaved, then more\n").unwrap();
    let synced = gft.run(&copy, &["sync"]);
    assert!(synced.contains("is up to date"), "{synced}");
    assert_eq!(gft.run(&copy, &["undo", "--list"]).lines().count(), 4);
}

#[test]
fn many_new_copies_at_once_and_dropping_the_project() {
    if !mounts() {
        return;
    }
    let (upstream, work, gft, _made) = project();
    let work = work.path().canonicalize().unwrap();
    let home = work.join("app");
    // Upstream moved, so the first of them brings the default branch up to date.
    let newest = commit_upstream(upstream.path(), "later.txt", "later\n");
    std::thread::sleep(std::time::Duration::from_secs(11));
    let gft = Arc::new(gft);
    let runs: Vec<_> = (0..8)
        .map(|n| {
            let (gft, home) = (gft.clone(), home.clone());
            std::thread::spawn(move || gft.try_run(&home, &["new", &format!("c{n}")]))
        })
        .collect();
    for run in runs {
        let out = run.join().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    for n in 0..8 {
        assert_eq!(git(&work.join(format!("app+c{n}")), &["rev-parse", "HEAD"]), newest);
    }

    // Dropping the project keeps copies with work that never landed, and work in
    // the home copy, unless forced.
    std::fs::write(work.join("app+c0/work.txt"), "work\n").unwrap();
    std::fs::write(home.join("home.txt"), "work at home\n").unwrap();
    let kept = gft.try_run(&home, &["drop"]);
    assert!(String::from_utf8_lossy(&kept.stderr).contains("c0 has 1 file that never landed"));
    assert!(work.join("app+c1").join("README.md").exists());
    gft.run(&home, &["rm", "--force", "c0"]);
    let kept = gft.try_run(&home, &["drop"]);
    assert!(String::from_utf8_lossy(&kept.stderr).contains("has 1 file changed in it"));
    assert!(work.join("app+c1").join("README.md").exists());
    gft.run(&work, &["drop", "o/n", "--force"]);
    assert!(!home.exists(), "home left");
    assert!(!work.join("app+c1").exists(), "copy left");
    assert!(String::from_utf8_lossy(&gft.try_run(&work, &["ls"]).stderr).contains("start with `gft get"));
    // And it can start over.
    let url = format!("file://{}", upstream.path().display());
    gft.run(&work, &["get", &url, "app", "--forge", "github:o/n"]);
    let again = gft.try_run(&work, &["get", &url, "again", "--forge", "github:o/n"]);
    assert!(String::from_utf8_lossy(&again.stderr).contains("o/n is a project already, at"));
    // Another owner's repository of the same name sits beside it.
    gft.run(&work, &["get", &url, "fork", "--forge", "github:p/n"]);
    gft.run(&work.join("fork"), &["new", "x"]);
    assert!(work.join("fork+x/README.md").exists());
    gft.run(&work, &["drop", "fork"]);
    assert!(!work.join("fork+x").exists(), "fork's copy left");
    assert!(work.join("app/README.md").exists(), "o/n went too");
}

#[test]
fn copies_come_back_and_follow_upstream() {
    if !mounts() {
        return;
    }
    let (upstream, work, gft, made) = project();
    let repo = upstream.path();
    let work = work.path().canonicalize().unwrap();
    let home = work.join("app");
    let at = |name: &str| work.join(format!("app+{name}"));
    gft.run(&home, &["new", "a", "b"]);

    // Unmounted, as after a restart: `ls` says so, a command in the copy mounts it
    // again, and `up` mounts the rest.
    let sandfs = sandfs::Home::at(gft.sandfs_home.path()).unwrap();
    sandfs.unmount("n.a").unwrap();
    sandfs.unmount("n.b").unwrap();
    assert!(gft.run(&home, &["ls"]).contains("not mounted: `gft up` brings it back"));
    let status = gft.try_run(&at("a"), &["status"]);
    assert!(String::from_utf8_lossy(&status.stderr).contains("mounted a again"));
    assert!(at("a").join("README.md").exists());
    assert!(gft.run(&home, &["up"]).contains("mounted b at "));
    assert_eq!(gft.run(&home, &["up"]), "all mounted\n");

    // Commits made in a copy are set aside on sync; their changes stay.
    std::fs::write(at("a").join("notes.txt"), "noted\n").unwrap();
    git(&at("a"), &["add", "-A"]);
    git(&at("a"), &["commit", "-q", "-m", "local"]);
    // Upstream renames a file the copy changed: the change follows it.
    std::fs::write(at("a").join("README.md"), "one\ntwo\nthree, edited\n").unwrap();
    std::fs::write(repo.join("README.md"), "zero\none\ntwo\nthree\n").unwrap();
    git(repo, &["mv", "README.md", "GUIDE.md"]);
    git(repo, &["commit", "-qam", "rename"]);
    let synced = gft.run(&at("a"), &["sync"]);
    assert!(synced.contains("set aside 1 commit made in the copy"), "{synced}");
    assert!(synced.contains("renamed upstream to GUIDE.md; merged there\tREADME.md"), "{synced}");
    assert!(!at("a").join("README.md").exists());
    assert_eq!(std::fs::read_to_string(at("a").join("GUIDE.md")).unwrap(), "zero\none\ntwo\nthree, edited\n");
    assert_eq!(gft.run(&at("a"), &["changes"]), "M GUIDE.md\nA notes.txt\n");
    let listed: Value = serde_json::from_str(&gft.run(&at("a"), &["changes", "--json"])).unwrap();
    assert_eq!(listed, json!([{"kind": "M", "path": "GUIDE.md"}, {"kind": "A", "path": "notes.txt"}]));

    // `diff` shows what landing sends, added files included; between copies, from
    // the first to the second.
    let diff = gft.run(&at("a"), &["diff"]);
    assert!(diff.contains("+three, edited") && diff.contains("+++ b/notes.txt"), "{diff}");
    let between = gft.run(&home, &["diff", "b", "a"]);
    assert!(between.contains("+noted") && between.contains("app+a/GUIDE.md"), "{between}");

    // What lands is the files, not the commit made in the copy, which stays as it was.
    let local = git(&at("a"), &["rev-parse", "HEAD"]);
    gft.run(&at("a"), &["land"]);
    {
        let landed = branch(repo, "graft/a");
        assert_eq!(file_at(repo, &landed, "notes.txt").unwrap(), "noted\n");
        assert_eq!(parents(repo, &landed), [git(repo, &["rev-parse", "main"])]);
        assert_eq!(git(&at("a"), &["rev-parse", "HEAD"]), local);
        assert_eq!(made.lock().unwrap().pulls.len(), 1);
    }

    // A copy made from another has its files: no overlap until one changes them.
    gft.run(&home, &["new", "c", "--from", "a"]);
    let listed: Value = serde_json::from_str(&gft.run(&home, &["ls", "--json"])).unwrap();
    let copies = listed[0]["copies"].as_array().unwrap();
    let c = copies.iter().find(|copy| copy["name"] == "c").unwrap();
    assert_eq!(c["overlaps"], json!({}), "{listed}");
    std::fs::write(at("c").join("notes.txt"), "noted differently\n").unwrap();
    assert!(gft.run(&home, &["ls"]).contains("overlaps a on notes.txt"));
    // Unmounted, their files can't be compared, so the overlap stands.
    sandfs.unmount("n.a").unwrap();
    sandfs.unmount("n.c").unwrap();
    let listed = gft.run(&home, &["ls"]);
    assert!(listed.contains("overlaps a on GUIDE.md and 1 more"), "{listed}");
    gft.run(&home, &["up"]);

    // Work done in the home copy shows, can be copied, and the home syncs too.
    std::fs::write(home.join("home.txt"), "made at home\n").unwrap();
    let status = gft.run(&home, &["status"]);
    assert!(status.contains("changed    1 file here, which new copies don't have"), "{status}");
    // Copied from another copy while the home copy is unmounted, it mounts again.
    sandfs.unmount("n").unwrap();
    gft.run(&at("a"), &["new", "h", "--from", "app"]);
    assert_eq!(std::fs::read_to_string(at("h").join("home.txt")).unwrap(), "made at home\n");
    let newest = commit_upstream(repo, "later.txt", "later\n");
    assert!(gft.run(&home, &["sync"]).contains("app moved from"));
    assert_eq!(git(&home, &["rev-parse", "HEAD"]), newest);
    assert!(home.join("later.txt").exists() && home.join("home.txt").exists());

    // Conflicts in the home copy keep `sync` exiting with 2 until settled.
    std::fs::write(home.join("GUIDE.md"), "zero\none at home\ntwo\nthree\n").unwrap();
    commit_upstream(repo, "GUIDE.md", "zero\none upstream\ntwo\nthree\n");
    assert_eq!(gft.try_run(&home, &["sync"]).status.code(), Some(2));
    let again = gft.try_run(&home, &["sync"]);
    assert_eq!(again.status.code(), Some(2), "{}", String::from_utf8_lossy(&again.stdout));

    // A named copy is mounted before its diff is read.
    sandfs.unmount("n.c").unwrap();
    assert!(gft.run(&home, &["diff", "c"]).contains("+noted differently"));
}

#[test]
fn any_remote_takes_landings_and_gitlab_takes_merge_requests() {
    if !mounts() {
        return;
    }
    let upstream = tempfile::Builder::new().prefix("graft-upstream").tempdir().unwrap();
    let repo = upstream.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(repo.join("README.md"), "one\ntwo\nthree\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "first"]);
    let (api, made) = fake_github(repo);
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    let url = format!("file://{}", repo.display());

    // No forge: landing pushes the branch, and nothing else.
    let got = gft.run(&work, &["get", &url, "plain"]);
    assert!(got.contains("(main)"), "{got}");
    let home = work.join("plain");
    gft.run(&home, &["new", "fix"]);
    let fix = work.join("plain+fix");
    std::fs::write(fix.join("README.md"), "one\ntwo, fixed\nthree\n").unwrap();
    assert!(gft.run(&fix, &["land", "-n"]).contains("pull request  new or open one, against main"));
    let landed = gft.run(&fix, &["land", "-m", "Fix two"]);
    let one = branch(repo, "graft/fix");
    assert_eq!(landed, format!("graft/fix at {}, 1 file\n", &one[..10]));
    assert_eq!(file_at(repo, &one, "README.md").unwrap(), "one\ntwo, fixed\nthree\n");
    assert!(made.lock().unwrap().pulls.is_empty());
    assert!(String::from_utf8_lossy(&gft.try_run(&fix, &["land"]).stderr).contains("nothing new to land"));
    // Merged on the remote by hand (squashed, say): sync sees its files there, takes
    // the merge in without a conflict, and the next landing starts a new branch.
    std::fs::write(repo.join("README.md"), "one\ntwo, fixed\nthree\n").unwrap();
    git(repo, &["commit", "-q", "-am", "Fix two (squashed)"]);
    commit_upstream(repo, "other.txt", "other\n");
    let synced = gft.run(&fix, &["sync"]);
    assert!(synced.contains("graft/fix is in main (main has its files)"), "{synced}");
    assert!(!synced.contains("conflict"), "{synced}");
    assert_eq!(gft.run(&fix, &["changes"]), "");
    // Its branch goes when asked, and only while it is where the landing left it.
    let removed = gft.run(&home, &["rm", "--delete-branch", "fix"]);
    assert!(removed.contains("fix: never landed, so no branch"), "{removed}");
    gft.run(&home, &["new", "keep"]);
    std::fs::write(work.join("plain+keep/keep.txt"), "keep\n").unwrap();
    gft.run(&work.join("plain+keep"), &["land"]);
    push_there(repo, "graft/keep", "theirs.txt", "theirs\n");
    let removed = gft.run(&home, &["rm", "--delete-branch", "keep"]);
    assert!(removed.contains("keep: kept branch graft/keep: it has commits this copy didn't land"), "{removed}");
    gft.run(&home, &["new", "gone"]);
    std::fs::write(work.join("plain+gone/gone.txt"), "gone\n").unwrap();
    gft.run(&work.join("plain+gone"), &["land"]);
    let removed = gft.run(&home, &["rm", "--delete-branch", "gone"]);
    assert!(removed.contains("gone: deleted branch graft/gone"), "{removed}");
    assert!(!branches(repo).contains_key("graft/gone"));
    gft.run(&work, &["drop", "plain", "--force"]);

    // GitLab: the same landing, with a merge request.
    gft.run(&work, &["get", &url, "lab", "--forge", "gitlab:o/n"]);
    let home = work.join("lab");
    gft.run(&home, &["new", "notes"]);
    let notes = work.join("lab+notes");
    std::fs::write(notes.join("notes.txt"), "notes\n").unwrap();
    let landed = gft.run(&notes, &["land", "-m", "Add notes", "--draft"]);
    assert!(landed.contains("https://gitlab.com/o/n/-/merge_requests/1"), "{landed}");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.pulls[0]["title"], "Draft: Add notes");
        assert_eq!(made.pulls[0]["base"], "main");
    }
    let preview = gft.run(&notes, &["land", "-n"]);
    assert!(preview.contains("merge request https://gitlab.com/o/n/-/merge_requests/1 (updated)"), "{preview}");
    std::fs::write(notes.join("notes.txt"), "notes, more\n").unwrap();
    gft.run(&notes, &["land", "--body", "More."]);
    assert_eq!(made.lock().unwrap().patches, [(1, json!("More.\n\nLanded with `gft land` from the copy `notes`."))]);
    // Merged there: sync says so, and landing after starts a new merge request.
    let merge = commit_upstream(repo, "notes.txt", "notes, more\n");
    merge_pull(&mut made.lock().unwrap(), 1, &merge);
    let synced = gft.run(&notes, &["sync"]);
    assert!(synced.contains("merge request #1 was merged; notes lands next as a new one"), "{synced}");
    std::fs::write(notes.join("notes.txt"), "notes, more, again\n").unwrap();
    let landed = gft.run(&notes, &["land"]);
    assert!(landed.contains("graft/notes-2 at ") && landed.contains("merge_requests/2"), "{landed}");
    assert_eq!(parents(repo, &branch(repo, "graft/notes-2")), [merge]);
}
