//! `gft` end to end: copies of a repository served from a folder, mounted by sandfs,
//! landing on a small stand-in for GitHub's API.
//!
//! Needs a working mount: on Linux FUSE 3, on macOS nothing. Skipped without one,
//! unless `GRAFT_REQUIRE_MOUNT` is set, as CI does.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
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

/// What the stand-in GitHub was asked to make.
#[derive(Default)]
struct Made {
    blobs: Vec<Vec<u8>>,
    trees: Vec<Value>,
    commits: Vec<Value>,
    refs: BTreeMap<String, String>,
    pulls: Vec<Value>,
    /// The bodies of PATCH pulls/N, as `(N, body)`.
    patches: Vec<(u64, Value)>,
    /// Fail the next branch update without making it, as if cut short before.
    fail_ref: bool,
    /// Make the next branch update but answer with a failure, as if cut short after.
    lose_ref: bool,
    /// Fail the next pull request without making it.
    fail_pull: bool,
    /// Make the next pull request but answer with a failure.
    lose_pull: bool,
    /// The repository the stand-in serves, for what GitHub reads from commits.
    repo: Option<std::path::PathBuf>,
    /// Trees and blobs made, by id.
    tree_ids: BTreeMap<String, Value>,
    blob_ids: BTreeMap<String, Vec<u8>>,
    /// Pull requests moved to another base, as `(N, base)`.
    retargets: Vec<(u64, String)>,
}

/// The bytes of `path` in the tree `tree`: one made by landing, else a commit's of
/// the repository, `tree-of-COMMIT`.
fn file_in(made: &Made, tree: &str, path: &str) -> Option<Vec<u8>> {
    if let Some(commit) = tree.strip_prefix("tree-of-") {
        let out = Command::new("git")
            .arg("-C")
            .arg(made.repo.as_ref()?)
            .args(["show", &format!("{commit}:{path}")])
            .output()
            .ok()?;
        return out.status.success().then_some(out.stdout);
    }
    let body = made.tree_ids.get(tree)?;
    match body["tree"].as_array()?.iter().find(|entry| entry["path"] == path) {
        Some(entry) => made.blob_ids.get(entry["sha"].as_str()?).cloned(),
        None => file_in(made, body["base_tree"].as_str()?, path),
    }
}

/// A pull request as GitHub answers for it.
fn pull_json(made: &Made, n: usize) -> Value {
    let pull = &made.pulls[n - 1];
    json!({"number": n, "html_url": format!("https://github.com/o/n/pull/{n}"),
        "state": pull.get("state").cloned().unwrap_or(json!("open")),
        "merged_at": pull.get("merged_at").cloned().unwrap_or(Value::Null),
        "merge_commit_sha": pull.get("merge_commit_sha").cloned().unwrap_or(Value::Null),
        "head": {"ref": pull["head"], "sha": pull.get("head_sha").cloned().unwrap_or(Value::Null)},
        "base": {"ref": pull["base"]}})
}

/// Keep the last commit of the branch `branch` with each pull request from it.
fn track_heads(made: &mut Made, branch: &str, sha: &str) {
    for pull in made.pulls.iter_mut().filter(|pull| pull["head"] == branch) {
        pull["head_sha"] = json!(sha);
    }
}

/// Whether `to` has `from` in its history: through the commits landing made, then
/// the repository's.
fn is_ancestor(made: &Made, from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    if let Some(commit) = made.commits.iter().find(|commit| commit["sha"] == to) {
        let parents = commit["parents"].as_array().cloned().unwrap_or_default();
        return parents.iter().any(|parent| is_ancestor(made, from, parent.as_str().unwrap_or_default()));
    }
    Command::new("git")
        .arg("-C")
        .arg(made.repo.as_ref().unwrap())
        .args(["merge-base", "--is-ancestor", from, to])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Answers the calls `gft land` makes, keeping what it was sent.
fn fake_github() -> (String, Arc<Mutex<Made>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let made = Arc::new(Mutex::new(Made::default()));
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
            let path = path.split_once("/repos/o/n").unwrap().1.trim_start_matches('/').to_owned();
            let mut made = state.lock().unwrap();
            let (status, answer) = match (method.as_str(), path.as_str()) {
                ("GET", p) if p.starts_with("git/commits/") => {
                    let id = &p["git/commits/".len()..];
                    let tree = made
                        .commits
                        .iter()
                        .position(|c| c["sha"] == id)
                        .map_or(format!("tree-of-{id}"), |n| made.commits[n]["tree"].as_str().unwrap().to_owned());
                    (200, json!({"sha": id, "tree": {"sha": tree}}))
                }
                ("GET", "") => (200, json!({"default_branch": "main"})),
                ("POST", "git/blobs") => {
                    use base64::Engine;
                    let data =
                        base64::engine::general_purpose::STANDARD.decode(body["content"].as_str().unwrap()).unwrap();
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    data.hash(&mut hasher);
                    let sha = format!("blob-{:x}", hasher.finish());
                    made.blob_ids.insert(sha.clone(), data.clone());
                    made.blobs.push(data);
                    (201, json!({"sha": sha}))
                }
                ("POST", "git/trees") => {
                    // Named by contents, as git names trees.
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    body.to_string().hash(&mut hasher);
                    let sha = format!("tree-{:x}", hasher.finish());
                    made.tree_ids.insert(sha.clone(), body.clone());
                    made.trees.push(body);
                    (201, json!({"sha": sha}))
                }
                ("POST", "git/commits") => {
                    let sha = format!("commit-{}", made.commits.len() + 1);
                    let mut commit = body;
                    commit["sha"] = json!(sha);
                    made.commits.push(commit);
                    (201, json!({"sha": sha}))
                }
                ("GET", p) if p.starts_with("git/ref/heads/") => match made.refs.get(&p["git/ref/".len()..]) {
                    Some(sha) => (200, json!({"object": {"sha": sha}})),
                    None => (404, json!({"message": "Not Found"})),
                },
                ("POST", "git/refs") | ("PATCH", _) if std::mem::take(&mut made.fail_ref) => {
                    (502, json!({"message": "cut short"}))
                }
                ("POST", "git/refs") => {
                    let name = body["ref"].as_str().unwrap().trim_start_matches("refs/").to_owned();
                    let sha = body["sha"].as_str().unwrap().to_owned();
                    track_heads(&mut made, name.trim_start_matches("heads/"), &sha);
                    made.refs.insert(name, sha);
                    if std::mem::take(&mut made.lose_ref) {
                        (502, json!({"message": "cut short"}))
                    } else {
                        (201, json!({}))
                    }
                }
                ("PATCH", p) if p.starts_with("git/refs/") => {
                    let sha = body["sha"].as_str().unwrap().to_owned();
                    track_heads(&mut made, p["git/refs/heads/".len()..].trim_end(), &sha);
                    made.refs.insert(p["git/refs/".len()..].to_owned(), sha);
                    if std::mem::take(&mut made.lose_ref) {
                        (502, json!({"message": "cut short"}))
                    } else {
                        (200, json!({}))
                    }
                }
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
                    let ancestor = is_ancestor(&made, from, to);
                    let status = match (from == to, ancestor) {
                        (true, _) => "identical",
                        (false, true) => "ahead",
                        (false, false) => "diverged",
                    };
                    (200, json!({"status": status}))
                }
                ("GET", p) if p.starts_with("contents/") => {
                    let (path, commit) = p["contents/".len()..].split_once("?ref=").unwrap();
                    let tree = made
                        .commits
                        .iter()
                        .find(|c| c["sha"] == commit)
                        .map_or(format!("tree-of-{commit}"), |c| c["tree"].as_str().unwrap().to_owned());
                    match file_in(&made, &tree, path) {
                        Some(data) => {
                            use base64::Engine;
                            let content = base64::engine::general_purpose::STANDARD.encode(data);
                            (200, json!({"type": "file", "content": content, "sha": "unused"}))
                        }
                        None => (404, json!({"message": "Not Found"})),
                    }
                }
                ("POST", "pulls") if std::mem::take(&mut made.fail_pull) => (502, json!({"message": "cut short"})),
                ("POST", "pulls") => {
                    let mut body = body;
                    body["head_sha"] = json!(made.refs.get(&format!("heads/{}", body["head"].as_str().unwrap())));
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
                    (200, json!({"number": number}))
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
            .env("GITHUB_TOKEN", "test-token")
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
            .env("GITHUB_TOKEN", "test-token")
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

    let (api, made) = fake_github();
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    let url = format!("file://{}", repo.display());
    let got = gft.run(&work, &["get", &url, "app", "--github", "o/n"]);
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
    assert_eq!(gft.run(&home, &["status"]).lines().last(), Some("copies   2"));
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
    assert!(landed.contains("graft/fix at commit-1, 2 files: https://github.com/o/n/pull/1"), "{landed}");
    assert!(gft.run(&fix, &["status"]).contains("pull request  https://github.com/o/n/pull/1\n"));
    {
        let made = made.lock().unwrap();
        assert_eq!(made.trees[0]["base_tree"], format!("tree-of-{second}"));
        let paths: Vec<&str> =
            made.trees[0]["tree"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
        assert_eq!(paths, ["notes.txt", "src/lib.rs"]);
        assert_eq!(made.commits[0]["parents"], json!([second]));
        assert_eq!(made.refs["heads/graft/fix"], "commit-1");
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
        assert_eq!(made.commits[1]["parents"], json!(["commit-1"]));
        assert_eq!(made.commits[1]["message"], "Update fix");
        assert_eq!(made.refs["heads/graft/fix"], "commit-2");
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
    let redescribed = gft.run(&fix, &["land", "--body", "Only words."]);
    assert!(redescribed.contains("described it anew"), "{redescribed}");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.patches[2], (1, json!("Only words.\n\nLanded with `gft land` from the copy `fix`.")));
        assert_eq!(made.refs["heads/graft/fix"], "commit-4", "no new commit");
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
    let (api, _) = fake_github();
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

    // A copy that changed a file the folder has changed since lands nothing.
    std::fs::write(other.join("todo.md"), "- a\n- c\n").unwrap();
    std::fs::write(other.join("new.md"), "new\n").unwrap();
    let clash = gft.try_run(&other, &["land"]);
    assert!(!clash.status.success());
    assert!(String::from_utf8_lossy(&clash.stderr).contains("todo.md"), "{}", String::from_utf8_lossy(&clash.stderr));
    assert!(!folder.join("new.md").exists(), "nothing was written");
    // After a sync it is a change on both sides, kept as the copy has it.
    let synced = gft.run(&other, &["sync"]);
    assert!(synced.contains("kept yours\ttodo.md") && synced.contains("1 need a look"), "{synced}");
    gft.run(&other, &["land"]);
    assert_eq!(std::fs::read_to_string(folder.join("todo.md")).unwrap(), "- a\n- c\n");
    assert_eq!(std::fs::read_to_string(folder.join("new.md")).unwrap(), "new\n");
    gft.run(&folder, &["rm", "other"]);
    assert!(!gft.run(&folder, &["ls"]).contains("other"));

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
    let (api, made) = fake_github();
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    gft.run(&work, &["get", &format!("file://{}", repo.display()), "app", "--github", "o/n"]);
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
    assert!(made.lock().unwrap().commits.is_empty() && made.lock().unwrap().refs.is_empty());

    // The branch moves but GitHub's answer is lost: landing again sees its own commit
    // there and goes on to the pull request, without a second commit.
    made.lock().unwrap().lose_ref = true;
    fails("fix", &["land", "-m", "Fix"]);
    assert_eq!(made.lock().unwrap().refs["heads/graft/fix"], "commit-1");
    let landed = gft.run(&at("fix"), &["land", "-m", "Fix"]);
    assert!(landed.contains("graft/fix at commit-1, 1 file: https://github.com/o/n/pull/1"), "{landed}");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.commits.len(), 1);
        assert_eq!(made.pulls.len(), 1);
        assert_eq!(made.pulls[0]["title"], "Fix");
    }
    // The commit is made but the branch never moves to it: landing again starts afresh.
    made.lock().unwrap().fail_ref = true;
    fails("two", &["land"]);
    assert!(!made.lock().unwrap().refs.contains_key("heads/graft/two"));
    gft.run(&at("two"), &["land"]);
    {
        let made = made.lock().unwrap();
        assert_eq!(made.refs["heads/graft/two"], "commit-3");
        assert_eq!(made.commits[2]["parents"], json!([first]));
    }
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
        // One each, and two's first, which never reached its branch.
        assert_eq!(made.commits.len(), 5);
    }
    // A branch of that name that this copy never landed is someone else's, even with
    // the same files, or an earlier copy's of the same name: a first landing takes a
    // name of its own and none of that branch's history.
    made.lock().unwrap().refs.insert("heads/graft/taken".into(), "commit-1".into());
    let landed = gft.run(&at("taken"), &["land"]);
    assert!(landed.contains("graft/taken-2 at "), "{landed}");
    {
        let made = made.lock().unwrap();
        assert_eq!(made.refs["heads/graft/taken"], "commit-1");
        assert_eq!(made.commits.last().unwrap()["parents"], json!([first]));
    }
    gft.run(&home, &["rm", "taken"]);

    // Someone adds a commit to fix's branch on GitHub: landing over it would undo it.
    {
        let mut made = made.lock().unwrap();
        made.commits.push(json!({"sha": "pushed", "tree": "tree-pushed"}));
        made.refs.insert("heads/graft/fix".into(), "pushed".into());
    }
    std::fs::write(at("fix").join("notes.txt"), "fix, and more\n").unwrap();
    let said = fails("fix", &["land"]);
    assert!(said.contains("graft/fix on GitHub has moved since fix last landed"), "{said}");
    // Even put back as its base has it, fix lands over them when told to.
    std::fs::remove_file(at("fix").join("notes.txt")).unwrap();
    let said = fails("fix", &["land"]);
    assert!(said.contains("has moved since"), "{said}");
    let plan_tree = made.lock().unwrap().commits.len();
    gft.run(&at("fix"), &["land", "--force"]);
    {
        let mut made = made.lock().unwrap();
        assert_eq!(made.commits.len(), plan_tree + 1);
        assert_eq!(made.commits.last().unwrap()["tree"], format!("tree-of-{first}"));
        // And someone adds to the branch again.
        made.commits.push(json!({"sha": "pushed", "tree": "tree-pushed"}));
        made.refs.insert("heads/graft/fix".into(), "pushed".into());
    }
    std::fs::write(at("fix").join("notes.txt"), "fix, and more\n").unwrap();
    let plan = gft.run(&at("fix"), &["land", "--force", "--dry-run"]);
    assert!(plan.contains("(moved from pushed, replacing commits fix didn't land)"), "{plan}");
    assert_eq!(made.lock().unwrap().refs["heads/graft/fix"], "pushed");
    gft.run(&at("fix"), &["land", "--force"]);
    let forced = made.lock().unwrap().refs["heads/graft/fix"].clone();
    assert_eq!(made.lock().unwrap().commits.last().unwrap()["parents"], json!(["pushed"]));

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
    assert_ne!(made.lock().unwrap().refs["heads/graft/fix"], forced);
    std::fs::write(at("child").join("child.txt"), "on top\n").unwrap();
    let plan = gft.run(&at("child"), &["land", "-n"]);
    assert!(plan.contains(&format!("commit        on {}\n", &forced[..10.min(forced.len())])), "{plan}");
    assert!(plan.contains("against graft/fix\n"), "{plan}");
    gft.run(&at("child"), &["land"]);
    {
        let made = made.lock().unwrap();
        assert_eq!(made.commits.last().unwrap()["parents"], json!([forced]));
        let pull = made.pulls.last().unwrap();
        assert_eq!((&pull["head"], &pull["base"]), (&json!("graft/child"), &json!("graft/fix")));
    }
    // Synced onto a newer commit, it lands with that commit as a second parent.
    std::fs::write(repo.join("README.md"), "hello again\n").unwrap();
    git(repo, &["commit", "-q", "-am", "second"]);
    let second = git(repo, &["rev-parse", "HEAD"]);
    gft.run(&at("child"), &["sync"]);
    std::fs::write(at("child").join("child.txt"), "on top, synced\n").unwrap();
    let before = made.lock().unwrap().refs["heads/graft/child"].clone();
    gft.run(&at("child"), &["land"]);
    assert_eq!(made.lock().unwrap().commits.last().unwrap()["parents"], json!([before, second]));

    // fix put back as its base has it: still a change, which `rm` keeps until landed.
    std::fs::remove_file(at("fix").join("notes.txt")).unwrap();
    assert_eq!(gft.run(&at("fix"), &["changes"]), "");
    let said = String::from_utf8_lossy(&gft.try_run(&home, &["rm", "fix"]).stderr).into_owned();
    assert!(said.contains("fix has 1 file that never landed"), "{said}");
    let landed = gft.run(&at("fix"), &["land"]);
    assert!(landed.contains("0 files"), "{landed}");
    assert_eq!(made.lock().unwrap().commits.last().unwrap()["tree"], format!("tree-of-{first}"));
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
    let (api, made) = fake_github();
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    git(&work, &["clone", "-q", &format!("file://{}", repo.display()), "app"]);
    let app = work.join("app");
    std::fs::write(app.join("README.md"), "hello, edited\n").unwrap();
    std::fs::write(app.join("new.txt"), "new\n").unwrap();
    let head = git(&app, &["rev-parse", "HEAD"]);

    // On a branch of its own, with no word from origin on its default branch:
    // GitHub says which it is.
    git(&app, &["remote", "set-head", "origin", "--delete"]);
    git(&app, &["checkout", "-q", "-b", "feature"]);
    // Its origin isn't on GitHub, so it needs saying where copies land.
    let refused = gft.try_run(&work, &["get", "./app"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("isn't on GitHub"));
    gft.run(&work, &["get", "./app", "--github", "o/n"]);
    assert!(gft.run(&app, &["ls"]).contains("(checkout, main)"));

    // A copy has the checkout's uncommitted work; the checkout keeps it too.
    gft.run(&app, &["new", "try"]);
    let copy = work.join("app+try");
    assert_eq!(std::fs::read_to_string(copy.join("README.md")).unwrap(), "hello, edited\n");
    assert_eq!(gft.run(&copy, &["changes"]), "M README.md\nA new.txt\n");
    assert_eq!(std::fs::read_to_string(app.join("README.md")).unwrap(), "hello, edited\n");
    std::fs::write(copy.join("more.txt"), "more\n").unwrap();
    gft.run(&copy, &["land"]);
    assert_eq!(made.lock().unwrap().commits[0]["parents"], json!([head]));

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
    let (api, made) = fake_github();
    made.lock().unwrap().repo = Some(repo.to_path_buf());
    let gft = Gft { sandfs_home: tempfile::tempdir().unwrap(), graft_home: tempfile::tempdir().unwrap(), api };
    let work = tempfile::tempdir().unwrap();
    gft.run(work.path(), &["get", &format!("file://{}", repo.display()), "app", "--github", "o/n"]);
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
    assert!(made.lock().unwrap().commits.is_empty());
    let plan = gft.run(&copy, &["land", "-n", "--allow-conflicts"]);
    assert!(plan.contains("M README.md"), "{plan}");
    // Settled, it lands.
    gft.run(&copy, &["save", "with markers"]);
    std::fs::write(copy.join("README.md"), "one\nTWO upstream\nthree\n").unwrap();
    assert!(!gft.run(&copy, &["status"]).contains("conflicts"));
    gft.run(&copy, &["land"]);
    assert_eq!(made.lock().unwrap().commits.len(), 1);
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
    assert_eq!(made.lock().unwrap().refs["heads/graft/closed"], "commit-1");
    let landed = gft.run(&at("closed"), &["land", "--new-pr", "--force"]);
    assert!(landed.contains("graft/closed-2 at ") && landed.contains("pull/3"), "{landed}");

    // Merged: what lands next needs to go on top of it, after a sync.
    let merge = commit_upstream(repo, "merged.txt", "first\n");
    {
        let mut made = made.lock().unwrap();
        made.pulls[1]["state"] = json!("closed");
        made.pulls[1]["merged_at"] = json!("2026-10-26T00:00:00Z");
        made.pulls[1]["merge_commit_sha"] = json!(merge);
    }
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
    let made = made.lock().unwrap();
    assert_eq!(made.commits.last().unwrap()["parents"], json!([merge]));
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
        made.pulls[0]["state"] = json!("closed");
        made.pulls[0]["merged_at"] = json!("2026-10-26T00:00:00Z");
        made.pulls[0]["merge_commit_sha"] = json!(squash);
        made.refs.remove("heads/graft/parent");
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
        assert_eq!(made.commits.last().unwrap()["parents"], json!([squash]));
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
    {
        let mut made = made.lock().unwrap();
        made.pulls[3]["state"] = json!("closed");
        made.pulls[3]["merged_at"] = json!("2026-10-26T00:00:00Z");
        made.pulls[3]["merge_commit_sha"] = json!(merged);
    }
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
    {
        let mut made = made.lock().unwrap();
        assert_eq!(made.pulls[6]["base"], "graft/base3");
        made.pulls[6]["state"] = json!("closed");
        made.pulls[6]["merged_at"] = json!("2026-10-26T00:00:00Z");
    }
    commit_upstream(repo, "elsewhere.txt", "z\n");
    let synced = gft.run(&work.join("app+mid"), &["sync"]);
    assert!(!synced.contains("new one") && !synced.contains("builds on main"), "{synced}");
    let refused = gft.try_run(&work.join("app+mid"), &["land"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("#7 was merged into graft/base3, not main"));
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
    gft.run(&work, &["get", &format!("file://{}", upstream.path().display()), "app", "--github", "o/n"]);
}
