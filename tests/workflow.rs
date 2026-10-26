//! `gft` end to end: copies of a repository served from a folder, mounted by sandfs,
//! landing on a small stand-in for GitHub's API.
//!
//! Needs `sandfs` (with `update`) and a working mount: on Linux FUSE 3, on macOS
//! nothing. Skipped when either is missing, unless `GRAFT_REQUIRE_SANDFS` is set, as
//! CI does. `GRAFT_SANDFS` names the sandfs binary to use.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

fn sandfs_available() -> bool {
    let sandfs = std::env::var_os("GRAFT_SANDFS").unwrap_or_else(|| "sandfs".into());
    let works = Command::new(&sandfs).args(["update", "--help"]).output().is_ok_and(|out| out.status.success());
    let mounts = cfg!(target_os = "macos")
        || (Path::new("/dev/fuse").exists()
            && Command::new("fusermount3").arg("--version").output().is_ok_and(|out| out.status.success()));
    if !(works && mounts) {
        assert!(std::env::var_os("GRAFT_REQUIRE_SANDFS").is_none(), "sandfs is required but can't mount here");
        eprintln!("skipping: no sandfs with `update`, or no mount");
    }
    works && mounts
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
                ("POST", "git/refs") => {
                    let name = body["ref"].as_str().unwrap().trim_start_matches("refs/").to_owned();
                    made.refs.insert(name, body["sha"].as_str().unwrap().to_owned());
                    (201, json!({}))
                }
                ("PATCH", p) if p.starts_with("git/refs/") => {
                    made.refs.insert(p["git/refs/".len()..].to_owned(), body["sha"].as_str().unwrap().to_owned());
                    (200, json!({}))
                }
                ("POST", "pulls") => {
                    made.pulls.push(body);
                    let n = made.pulls.len();
                    (201, json!({"number": n, "html_url": format!("https://github.com/o/n/pull/{n}")}))
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

    fn run(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.try_run(dir, args);
        assert!(out.status.success(), "gft {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Gft {
    fn drop(&mut self) {
        let sandfs = std::env::var_os("GRAFT_SANDFS").unwrap_or_else(|| "sandfs".into());
        let list = Command::new(&sandfs).env("SANDFS_HOME", self.sandfs_home.path()).arg("list").output();
        for line in list.map(|out| String::from_utf8_lossy(&out.stdout).into_owned()).unwrap_or_default().lines() {
            let name = line.split('\t').next().unwrap_or_default();
            let _ = Command::new(&sandfs).env("SANDFS_HOME", self.sandfs_home.path()).args(["unmount", name]).output();
        }
    }
}

#[test]
fn copies_are_made_compared_synced_and_landed() {
    if !sandfs_available() {
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
    git(repo, &["add", "-A"]);
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
    // Build output git ignores is no change, in either copy.
    for copy in [&fix, &try_] {
        std::fs::create_dir_all(copy.join("target/debug")).unwrap();
        std::fs::write(copy.join("target/debug/app"), "built\n").unwrap();
    }
    assert_eq!(gft.run(&fix, &["changes"]), "A notes.txt\nM src/lib.rs\n");
    assert_eq!(gft.run(&fix, &["changes", "fix", "try"]), "A notes.txt\nM src/lib.rs\n");
    let listed = gft.run(&home, &["ls"]);
    assert!(listed.contains("fix\t2 files\tbase ") && listed.contains("overlaps try on src/lib.rs"), "{listed}");
    assert_eq!(gft.run(&home, &["path", "fix"]), format!("{}\n", fix.display()));

    // A snapshot, a wrong turn, and back.
    gft.run(&fix, &["save", "two returns 2"]);
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
    gft.run(&home, &["undo", "--copy", "fix"]);

    // Upstream changes another line of the same file: sync merges it in.
    std::fs::write(repo.join("src/lib.rs"), "// lib\nfn one() {}\n\nfn two() {}\n").unwrap();
    std::fs::write(repo.join("CHANGES.md"), "- one\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "second"]);
    let second = git(repo, &["rev-parse", "HEAD"]);
    let synced = gft.run(&fix, &["sync"]);
    assert!(synced.contains("merged\tsrc/lib.rs"), "{synced}");
    assert_eq!(std::fs::read_to_string(fix.join("src/lib.rs")).unwrap(), "// lib\nfn one() {}\n\nfn two() { 2 }\n");
    assert_eq!(std::fs::read_to_string(fix.join("CHANGES.md")).unwrap(), "- one\n");
    assert_eq!(gft.run(&fix, &["changes"]), "A notes.txt\nM src/lib.rs\n");
    assert_ne!(first, second);

    // Landing makes a branch, one commit on the newest base, and a pull request.
    let landed = gft.run(&fix, &["land", "-m", "Make two return 2"]);
    assert!(landed.contains("graft/fix at commit-1, 2 files: https://github.com/o/n/pull/1"), "{landed}");
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
    }
    let unchanged = gft.try_run(&fix, &["land"]);
    assert!(String::from_utf8_lossy(&unchanged.stderr).contains("nothing new to land"));

    // A copy of a copy starts with its changes and lands on top of its branch.
    gft.run(&home, &["new", "more", "--from", "fix"]);
    let more = work.join("app+more");
    assert_eq!(std::fs::read_to_string(more.join("notes.txt")).unwrap(), "why, and how\n");

    gft.run(&home, &["rm", "try", "more"]);
    assert!(!gft.run(&home, &["ls"]).contains("try"));
    assert!(gft.try_run(&home, &["path", "try"]).stderr.ends_with(b"`gft ls` lists them\n"));
}
