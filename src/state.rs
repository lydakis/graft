//! What Graft remembers between commands: one file per project, under
//! `$GRAFT_HOME` (else `$XDG_DATA_HOME/graft`, else `~/.local/share/graft`).
//!
//! ```text
//! projects/<owner>/<repo>.json   the project, its copies and their snapshots
//! projects/<owner>/<repo>.lock   held while a command changes that file
//! ```
//!
//! The files themselves live in sandfs: every copy and snapshot is a sandfs
//! workspace, named `<repo>.<copy>` and `<repo>.<copy>.<n>`.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// A repository Graft works on, with its copies.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    /// `owner/name` on GitHub, where copies land.
    pub repo: String,
    /// What sandfs mounts: the repository's URL.
    pub source: String,
    /// The home copy's folder, which tracks the default branch.
    pub home: PathBuf,
    /// The sandfs workspace of the home copy.
    pub workspace: String,
    /// The default branch, which copies land onto.
    pub branch: String,
    #[serde(default)]
    pub copies: BTreeMap<String, Copy>,
}

/// One working copy: a sandfs workspace mounted at `dir`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Copy {
    pub dir: PathBuf,
    pub workspace: String,
    /// The commit of the default branch the copy sits on.
    pub base: String,
    /// The copy it was made from, if not the default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Its pull request, once landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<Pr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshots: Vec<Snapshot>,
    /// The number the next snapshot's workspace gets.
    #[serde(default)]
    pub next: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pr {
    pub number: u64,
    pub url: String,
    /// The branch on GitHub that holds the copy's commits.
    pub branch: String,
    /// The base commit the last landing was made on.
    pub landed_on: String,
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
        serde_json::from_slice(&data).with_context(|| format!("reading {}", path.display()))
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

    fn lock(&self, repo: &str) -> Result<File> {
        let path = self.file(repo, "lock");
        std::fs::create_dir_all(path.parent().expect("projects folder")).context("creating Graft's home")?;
        let file = File::options().write(true).create(true).truncate(false).open(&path)?;
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
                    found.push(serde_json::from_slice(&data).with_context(|| format!("reading {}", path.display()))?);
                }
            }
        }
        Ok(found)
    }

    /// The project whose home copy or one of whose copies holds `dir`, and that copy.
    pub fn find(&self, dir: &Path) -> Result<(Project, Option<String>)> {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        for project in self.projects()? {
            for (name, copy) in &project.copies {
                if dir.starts_with(&copy.dir) {
                    let name = name.clone();
                    return Ok((project, Some(name)));
                }
            }
            if dir.starts_with(&project.home) {
                return Ok((project, None));
            }
        }
        bail!("{} is in no Graft copy; start with `gft get OWNER/REPO`", dir.display())
    }
}

/// Copy names become folder and workspace names.
pub fn check_copy_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 60
        && !name.starts_with(['-', '_'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));
    if ok { Ok(()) } else { bail!("invalid copy name {name:?}: use letters, digits, '_' and '-'") }
}
