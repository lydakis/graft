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
            let path = path.split_once("/repos/o/n/").unwrap().1.to_owned();
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
                ("POST", "git/blobs") => {
                    use base64::Engine;
                    let data =
                        base64::engine::general_purpose::STANDARD.decode(body["content"].as_str().unwrap()).unwrap();
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    data.hash(&mut hasher);
                    made.blobs.push(data);
                    (201, json!({"sha": format!("blob-{:x}", hasher.finish())}))
                }
                ("POST", "git/trees") => {
                    // Named by contents, as git names trees.
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    body.to_string().hash(&mut hasher);
                    made.trees.push(body);
                    (201, json!({"sha": format!("tree-{:x}", hasher.finish())}))
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
                    made.refs.insert(name, body["sha"].as_str().unwrap().to_owned());
                    if std::mem::take(&mut made.lose_ref) {
                        (502, json!({"message": "cut short"}))
                    } else {
                        (201, json!({}))
                    }
                }
                ("PATCH", p) if p.starts_with("git/refs/") => {
                    made.refs.insert(p["git/refs/".len()..].to_owned(), body["sha"].as_str().unwrap().to_owned());
                    if std::mem::take(&mut made.lose_ref) {
                        (502, json!({"message": "cut short"}))
                    } else {
                        (200, json!({}))
                    }
                }
                ("GET", p) if p.starts_with("pulls?head=o:") => {
                    let branch = p["pulls?head=o:".len()..].split('&').next().unwrap();
                    let open: Vec<Value> = (1..=made.pulls.len())
                        .filter(|n| made.pulls[n - 1]["head"] == branch)
                        .map(|n| json!({"number": n, "html_url": format!("https://github.com/o/n/pull/{n}")}))
                        .collect();
                    (200, json!(open))
                }
                ("POST", "pulls") if std::mem::take(&mut made.fail_pull) => (502, json!({"message": "cut short"})),
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
                    made.patches.push((number, body["body"].clone()));
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
    let listed = gft.run(&home, &["undo", "--list", "--copy", "fix"]);
    let lines: Vec<&str> = listed.lines().collect();
    assert!(lines.len() == 2 && lines[0].starts_with("1\t") && lines[1].starts_with("2\t"), "{listed}");
    assert!(lines[0].ends_with(" sec ago\tagain") && lines[1].ends_with(" sec ago\ttwo returns 2"), "{listed}");
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
    // the same files.
    made.lock().unwrap().refs.insert("heads/graft/taken".into(), "commit-1".into());
    let said = fails("taken", &["land"]);
    assert!(said.contains("graft/taken on GitHub already exists"), "{said}");
    gft.run(&home, &["rm", "--force", "taken"]);

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

    // A copy of fix sits on fix's landing as it was: fix landing more since changes
    // neither what it holds nor what its own landing follows.
    gft.run(&home, &["new", "child", "--from", "fix"]);
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
