//! What Graft remembers between commands: one file per project, under
//! `$GRAFT_HOME` (else `$XDG_DATA_HOME/graft`, else `~/.local/share/graft`).
//!
//! ```text
//! projects/<owner>/<repo>.json   the project, its copies and their snapshots
//! projects/<owner>/<repo>.lock   held while a command changes that file
//! projects/<owner>/<repo>.main    held while new copies bring the default branch up to date
//! ```
//!
//! The files themselves live in sandfs: every copy and snapshot is a sandfs
//! workspace, named `<repo>.<copy>` and `<repo>.<copy>.<n>`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A repository or a plain folder Graft works on, with its copies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    /// The project's name, as commands take it: `owner/name` for a repository,
    /// `folder/name` for a plain folder.
    pub repo: String,
    /// Where copies land: the URL `git push` goes to. Empty for a plain folder.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remote: String,
    /// The forge that keeps review requests for the remote, if it has one Graft knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<crate::forge::Spec>,
    /// What sandfs mounts: the repository's URL, or the folder or checkout.
    pub source: String,
    /// The home copy's folder, which tracks the default branch; for a plain folder or
    /// a checkout, the folder itself.
    pub home: PathBuf,
    /// The sandfs workspace of the home copy, and the start of its copies' names.
    pub workspace: String,
    /// The default branch, which copies land onto; empty for a plain folder.
    pub branch: String,
    /// Whether the project is a plain folder, which copies land back into, rather than
    /// a repository on a remote.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub folder: bool,
    /// Whether the home copy is a git checkout on this machine, which copies start
    /// from (its commit and uncommitted changes) and sync with, rather than a mount
    /// of the default branch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checkout: bool,
    /// How many times copies of a plain folder took the folder as it was, which
    /// numbers their bases.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub takes: u64,
    /// When the default branch was last brought up to date for new copies, in seconds
    /// since the Unix epoch.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fetched: u64,
    #[serde(default)]
    pub copies: BTreeMap<String, Copy>,
}

/// One working copy: a sandfs workspace mounted at `dir`.
///
/// Three things say where a copy stands, and each answers one question: `base` is
/// what its files start from, `stacked` is the landed work it was made on top of,
/// and `landed` is what it last put on the remote. Landing builds on all three, so it
/// never undoes what it doesn't know about.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Copy {
    pub dir: PathBuf,
    pub workspace: String,
    /// What the copy's files start from, which `gft sync` moves: the commit of the
    /// default branch it sits on. For a copy of a plain folder, `N@SECONDS`: the
    /// project's Nth take of the folder as it was, SECONDS after the Unix epoch. Only
    /// N tells two takes apart; the time is for people.
    pub base: String,
    /// The copy it was made from, if not the default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The landing it was made on top of: when it was made from a copy that had
    /// landed, that copy's landing as it was then. Its review request is based on that
    /// branch, and its first commit follows that landing rather than the branch's
    /// newest commit, which the copy's files don't have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacked: Option<Landing>,
    /// What its last landing put on the remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed: Option<Landing>,
    /// A landing whose commit is made but whose branch may not have moved to it yet,
    /// kept until it has, so landing again after one cut short knows its own commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Landing>,
    /// Its pull or merge request, once made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<Pr>,
    /// Files `gft sync` left with conflict markers, which `land` refuses until they
    /// are gone.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub conflicts: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<Snapshot>,
    /// The number the next snapshot's workspace gets.
    #[serde(default)]
    pub next: u64,
}

/// A copy's changes as a landing left them on the remote.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Landing {
    /// The branch on the remote.
    pub branch: String,
    /// The commit the landing left the branch at.
    pub commit: String,
    /// The base commit that commit was made on.
    pub on: String,
    /// Each file the landing carried, as `fingerprint` saw it, for `gft rm`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    /// Where Graft before 0.1 kept its last landing, read only to move it to
    /// `Copy::landed`.
    #[serde(default, skip_serializing, rename = "branch")]
    old_branch: Option<String>,
    #[serde(default, skip_serializing, rename = "landed_on")]
    old_on: Option<String>,
    #[serde(default, skip_serializing, rename = "landed")]
    old_files: BTreeMap<String, String>,
}

impl Pr {
    pub fn new(number: u64, url: String) -> Pr {
        Pr { number, url, old_branch: None, old_on: None, old_files: BTreeMap::new() }
    }
}

/// A project as read from its file, with a landing that Graft before 0.1 kept with
/// the pull request moved to where it is kept now. That Graft didn't keep the commit
/// it left the branch at, so the commit is left empty, for `land` to take the
/// branch's as the copy's own. Graft before 0.3 landed on GitHub alone, through its
/// API, and kept no remote: such a project lands on its repository there.
fn read(data: &[u8]) -> serde_json::Result<Project> {
    let mut project: Project = serde_json::from_slice(data)?;
    if !project.folder && project.remote.is_empty() {
        project.remote = format!("https://github.com/{}", project.repo);
        project.forge = crate::forge::Spec::new(crate::forge::Kind::GitHub, "github.com", &project.repo);
    }
    for copy in project.copies.values_mut() {
        let Some(pr) = &mut copy.pr else { continue };
        if let (Some(branch), Some(on), None) = (pr.old_branch.take(), pr.old_on.take(), &copy.landed) {
            let files = std::mem::take(&mut pr.old_files);
            copy.landed = Some(Landing { branch, commit: String::new(), on, files });
        }
    }
    Ok(project)
}

/// A copy as it was at one moment, kept as an unmounted sandfs workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub workspace: String,
    pub base: String,
    pub note: String,
    /// Seconds since the Unix epoch.
    pub time: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Graft's home directory.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn locate() -> Result<Store> {
        let root = match (std::env::var_os("GRAFT_HOME"), std::env::var_os("XDG_DATA_HOME")) {
            (Some(home), _) => PathBuf::from(home),
            (None, Some(data)) => PathBuf::from(data).join("graft"),
            (None, None) => std::env::home_dir().context("no home folder")?.join(".local/share/graft"),
        };
        Ok(Store { root })
    }

    fn file(&self, repo: &str, ext: &str) -> PathBuf {
        self.root.join("projects").join(format!("{repo}.{ext}"))
    }

    pub fn exists(&self, repo: &str) -> bool {
        self.file(repo, "json").is_file()
    }

    pub fn load(&self, repo: &str) -> Result<Project> {
        let path = self.file(repo, "json");
        let data = std::fs::read(&path).with_context(|| format!("no project {repo}"))?;
        read(&data).with_context(|| format!("reading {}", path.display()))
    }

    fn save(&self, project: &Project) -> Result<()> {
        let path = self.file(&project.repo, "json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(project)?).context("saving the project")?;
        std::fs::rename(&tmp, &path).context("saving the project")
    }

    /// Change the project `repo` under its lock, reading it afresh first, so commands
    /// run at once (one per agent) don't lose each other's changes.
    pub fn edit<T>(&self, repo: &str, change: impl FnOnce(&mut Project) -> Result<T>) -> Result<T> {
        let _lock = self.lock(repo)?;
        let mut project = self.load(repo)?;
        let out = change(&mut project)?;
        self.save(&project)?;
        Ok(out)
    }

    /// Record a new project; one already recorded is refused.
    pub fn create(&self, project: &Project) -> Result<()> {
        let _lock = self.lock(&project.repo)?;
        if self.exists(&project.repo) {
            bail!("{} is already a project here; its home copy is listed by `gft ls`", project.repo);
        }
        self.save(project)
    }

    /// Take the right to bring the project's default branch up to date and copy it,
    /// waiting for any other command doing so.
    pub fn lock_main(&self, repo: &str) -> Result<File> {
        self.lock_file(&self.file(repo, "main"))
    }

    /// Forget the project `repo`.
    pub fn forget(&self, repo: &str) -> Result<()> {
        {
            let _lock = self.lock(repo)?;
            std::fs::remove_file(self.file(repo, "json")).with_context(|| format!("forgetting {repo}"))?;
        }
        for ext in ["lock", "main"] {
            let _ = std::fs::remove_file(self.file(repo, ext));
        }
        Ok(())
    }

    fn lock(&self, repo: &str) -> Result<File> {
        self.lock_file(&self.file(repo, "lock"))
    }

    fn lock_file(&self, path: &Path) -> Result<File> {
        std::fs::create_dir_all(path.parent().expect("projects folder")).context("creating Graft's home")?;
        let file = File::options().write(true).create(true).truncate(false).open(path)?;
        file.lock().with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    /// Every project, by `owner/name`.
    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut found = Vec::new();
        let Ok(owners) = std::fs::read_dir(self.root.join("projects")) else { return Ok(found) };
        for owner in owners.flatten() {
            for entry in std::fs::read_dir(owner.path())?.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "json") {
                    let data = std::fs::read(&path)?;
                    found.push(read(&data).with_context(|| format!("reading {}", path.display()))?);
                }
            }
        }
        Ok(found)
    }

    /// The project whose home copy or one of whose copies holds `dir`, and that copy.
    pub fn find(&self, dir: &Path) -> Result<(Project, Option<String>)> {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        // The deepest folder that holds `dir` wins, since a folder project may sit
        // inside another one.
        let mut found: Option<(usize, Project, Option<String>)> = None;
        for project in self.projects()? {
            let homes = project.copies.iter().map(|(name, copy)| (&copy.dir, Some(name.clone())));
            let best = homes
                .chain([(&project.home, None)])
                .filter(|(home, _)| dir.starts_with(home))
                .map(|(home, name)| (home.components().count(), name))
                .max_by_key(|(depth, _)| *depth);
            if let Some((depth, name)) = best
                && found.as_ref().is_none_or(|(deepest, ..)| depth > *deepest)
            {
                found = Some((depth, project, name));
            }
        }
        if let Some((_, project, name)) = found {
            return Ok((project, name));
        }
        bail!("{} is in no Graft copy; start with `gft get OWNER/REPO`", dir.display())
    }
}

/// Copy names become folder and workspace names.
pub fn check_copy_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("a copy needs a name; give letters, digits, '_' or '-'");
    }
    if let Some(c) = name.chars().find(|c| !(c.is_ascii_alphanumeric() || "_-".contains(*c))) {
        bail!("invalid copy name {name:?}: {c:?} isn't allowed; use only letters, digits, '_' and '-'");
    }
    if name.starts_with(['-', '_']) {
        bail!("invalid copy name {name:?}: it can't start with '-' or '_'; start with a letter or digit");
    }
    if name.len() > 60 {
        bail!("invalid copy name {name:?}: {} characters is too long; use at most 60", name.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_copy_name;

    #[test]
    fn a_landing_kept_with_the_pull_request_moves_to_the_copy() {
        let old = br#"{"repo": "o/n", "source": "s", "home": "/h", "workspace": "n", "branch": "main",
            "copies": {"fix": {"dir": "/h+fix", "workspace": "n.fix", "base": "b2",
                "pr": {"number": 3, "url": "u", "branch": "graft/fix", "landed_on": "b1",
                    "landed": {"a.txt": "100644 abc"}}}}}"#;
        let project = super::read(old).unwrap();
        let copy = &project.copies["fix"];
        let landed = copy.landed.as_ref().unwrap();
        assert_eq!((landed.branch.as_str(), landed.commit.as_str(), landed.on.as_str()), ("graft/fix", "", "b1"));
        assert_eq!(landed.files["a.txt"], "100644 abc");
        let saved = serde_json::to_value(&project).unwrap();
        assert_eq!(saved["copies"]["fix"]["pr"], serde_json::json!({"number": 3, "url": "u"}));
    }

    fn refusal(name: &str) -> String {
        check_copy_name(name).unwrap_err().to_string()
    }

    #[test]
    fn good_copy_names_pass() {
        for name in ["a", "fix-bug", "fix_bug", "Fix2", "9lives", &"x".repeat(60)] {
            assert!(check_copy_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn empty_name_is_refused() {
        assert!(refusal("").contains("needs a name"));
    }

    #[test]
    fn characters_other_than_letters_digits_dash_and_underscore_are_refused() {
        assert!(refusal("two words").contains("' ' isn't allowed"));
        assert!(refusal("a/b").contains("'/' isn't allowed"));
        assert!(refusal("a.b").contains("'.' isn't allowed"));
        assert!(refusal("caf\u{e9}").contains("isn't allowed"));
    }

    #[test]
    fn leading_dash_or_underscore_is_refused() {
        assert!(refusal("-x").contains("can't start with"));
        assert!(refusal("_x").contains("can't start with"));
        assert!(check_copy_name("x-").is_ok());
    }

    #[test]
    fn names_over_sixty_characters_are_refused() {
        assert!(check_copy_name(&"x".repeat(60)).is_ok());
        assert!(refusal(&"x".repeat(61)).contains("61 characters is too long"));
    }
}
