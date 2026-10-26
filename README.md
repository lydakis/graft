# Graft

Work in instant copies of a GitHub repository, or of a plain folder. Each copy is a [sandfs](https://github.com/lydakis/sandfs) folder over a commit of the default branch, so nothing is cloned, a copy appears in a fraction of a second, and it costs only what it changes. When a copy is good, `gft land` turns it into a branch and a pull request through GitHub's API, with no local commit and no push.

The command is `gft` (also installed as `graft`). For plain folders, see [Plain folders](#plain-folders).

```bash
gft get lydakis/sandfs              # ./sandfs: the home copy, tracking main, nothing downloaded up front
cd sandfs
gft new fix-unmount                 # ../sandfs+fix-unmount, on the newest commit of main
cd ../sandfs+fix-unmount
# edit, build, test as in any folder; git works here too, but nothing needs staging or committing
gft changes                         # A/M/D for each file this copy changed
gft changes --count                 # just the totals: 3 added, 5 modified, 1 deleted
gft save "busy unmount works"       # a snapshot
gft undo --list                     # the snapshots, newest first, numbered for `gft undo N`
gft undo                            # back to the last snapshot
gft sync                            # main moved on: move onto it, merging files changed on both sides
gft land -m "Recover from a busy unmount"   # branch graft/fix-unmount and a pull request
gft land                            # after review fixes: one more commit on the same pull request
gft land --body-file notes.md       # a description for the pull request (--body TEXT, or `-` for stdin)
gft rm fix-unmount
```

Many agents at once:

```bash
gft new agent-{01..20}              # twenty copies
gft ls                              # files changed per copy, and which copies changed the same files (outside a copy: every project)
gft changes agent-07 agent-03       # how two attempts differ
gft new agent-07b --from agent-07   # branch an attempt mid-task, keeping its changes
gft land agent-07 --draft
gft rm agent-{01..20} agent-07b       # refuses copies with changes that never landed
gft rm --force agent-07b            # delete one anyway, losing those changes
```

## Commands

| Command | What it does |
| --- | --- |
| `gft get OWNER/NAME [DIR] [--github OWNER/NAME]` | Mount the default branch at DIR (default `./NAME`) as the home copy. Takes a repository URL too; for one that isn't github.com's, `--github` says where copies land. Given a folder (`./notes`, `/data/survey`), the folder itself is the home copy (see [Plain folders](#plain-folders)). |
| `gft new NAME... [--from COPY]` | Copies on the newest commit (fetched once for all of them), or on another copy with its changes. They appear next to the home copy as `HOME+NAME`. |
| `gft ls` | Copies, files changed, the commit each sits on, pull requests, overlaps. Outside a copy, every project's. |
| `gft status` | In a copy: its name, folder, commit, files changed, snapshots and pull request. In the home copy: the project and how many copies exist. |
| `gft path NAME` | A copy's folder. |
| `gft changes [A] [B] [--count]` | What copy A changed, or how A differs from B. `--count` prints only the totals, like `3 added, 5 modified, 1 deleted`. |
| `gft save [NOTE]` / `gft undo [N]` | Snapshot the copy; go back N snapshots (default 1). Both take `-c COPY`. |
| `gft undo --list` | The copy's snapshots, newest first, each with its number for `undo N`, how long ago it was taken and its note. Changes nothing. |
| `gft sync [NAME]` | Move the copy onto the newest commit, in place: programs working in the copy carry on. Files changed on both sides are merged line by line with `git merge-file`; conflicts are left marked in the file and listed. |
| `gft land [NAME] [-m TITLE] [--body TEXT \| --body-file PATH] [--draft]` | Commit the copy's changes on branch `graft/NAME` on GitHub and open a pull request (against the copy it came from, if that one landed). Landing again adds a commit. The description is TEXT or the contents of PATH (`-` reads standard input), followed by a line saying it was landed with `gft land`. Giving one when landing again replaces the description on the open pull request; giving none leaves it as it is. |
| `gft rm [--force] NAME...` | Delete copies and their snapshots. Pull requests stay. A copy with changes that never landed (no pull request, or files changed since its last landing) is kept, and the files that would be lost are counted, unless `--force` is given. |

Commands that take a copy use the one the current folder is in when none is named.

## Many agents at once

Give each agent its own copy, named for its task, and let each land its own pull request. Nothing is shared between copies, so one agent's half-finished edit never breaks another's build, and the copies cost almost nothing to make.

```bash
gft new fix-timeout add-retries docs-env   # one copy per agent, made together
# each agent works only in its own folder (gft path NAME prints it)

# agent add-retries, before it starts on a file and again before it lands:
gft ls
#   fix-timeout   2 files  base 8479613b94  -  overlaps add-retries on src/net.rs
#   add-retries   3 files  base 8479613b94  -  overlaps fix-timeout on src/net.rs
#   docs-env      1 file   base 8479613b94  -
gft changes fix-timeout add-retries        # how the two copies differ

# a follow-up that needs fix-timeout's change underneath it:
gft new retry-docs --from fix-timeout

gft sync add-retries                       # main moved on
gft land add-retries -m "Retry on timeout"             # one pull request per copy
```

- **Check overlaps with `gft ls`.** Each copy's line ends with `overlaps OTHER on FILE` for every other copy that changed a file it also changed. It only sees changes made so far, so an agent runs it before it starts on a shared file and once more before it lands. `gft ls` lists every copy of the repository, so an agent reads its own line. An overlap isn't an error: either the agents split the work differently, or the later one expects to resolve a merge. `gft changes OTHER` lists the files the other copy changed, and `gft changes MINE OTHER` how the two differ.
- **Stack dependent work with `gft new NAME --from COPY`.** The new copy starts with that copy's changes, so the second agent builds on the first one's work without waiting for it to land. Independent work always starts from main (plain `gft new`), so it never inherits another agent's unfinished work.
- **Sync when main has moved and you are about to land, or when you need what landed.** Other agents' pull requests move main; a copy stays on the commit it started from until it syncs. Syncing is not needed to keep working, and an agent can sync from inside its copy while its programs keep running there. Files changed on both sides are merged line by line; any listed as conflicts have `<<<<<<<` markers to fix before you build, test and land.
- **Land each copy when it is good.** `gft land -m TITLE` opens `graft/NAME` as a pull request; `--draft` leaves it for review. A stacked copy's pull request is opened against its parent's branch once the parent has landed, so land the parent first and review in that order. Landing again after review comments adds a commit to the same pull request. When the pull requests are merged, `gft rm` the copies.

## Plain folders

A folder that isn't a git repository works the same way, with the folder itself as the home copy and nothing on GitHub:

```bash
gft get ~/data/survey               # the folder is the home copy; nothing is copied yet
cd ~/data/survey
gft new cleanup                     # ~/data/survey+cleanup: the folder as it is now
cd ../survey+cleanup
# edit as in any folder; later changes to ~/data/survey stay out of the copy
gft changes                         # A/M/D against the folder as it was copied
gft sync                            # take in what the folder has now, keeping the copy's changes
gft land                            # write the copy's changes into ~/data/survey
gft rm cleanup
```

- **A copy is the folder as it was when copied.** Its files are cloned, which costs no space for their contents, but needs the folder and sandfs's home on one disk that clones files: APFS on a Mac, btrfs or XFS on Linux (not ext4).
- **`land` writes the copy's changes into the folder**: changed and new files, and deletions. If the folder changed one of those files since the copy was made or last synced, nothing is written and the files are named; `gft sync`, then `land` again, writes the copy's version over the folder's. After landing, the copy sits on the folder as it is now, with no changes of its own.
- **`sync` has nothing to merge from**: a folder keeps no older versions, so a file changed both in the copy and in the folder keeps the copy's version and is listed.
- `save`, `undo`, `ls`, `changes` and `rm` work as for a repository. `land` takes no `-m`, `--body` or `--draft`.

## Needs

- A `sandfs` newer than 0.6.0, which moves a mounted workspace in place (until the next release, `cargo install --git https://github.com/lydakis/sandfs sandfs-cli`), and what it needs to mount: FUSE 3 on Linux, nothing on macOS.
- `git` and `curl`.
- A GitHub token for `land`: `GITHUB_TOKEN` or `GH_TOKEN`, else whatever `gh auth token` gives (see [Configuration](#configuration)).

## Limits

- `undo` back past a `sync` makes the copy again, unmounting it for a moment, so no program may be working inside it then (an agent's shell sitting in the folder counts). So does `rm`. Other undos, and `sync`, change the copy in place and need nothing stopped.
- Landing uploads each changed file as one API call. A few hundred changed files take a while, and GitHub allows 5,000 calls an hour per token.
- Repositories on GitHub (or a stand-in for its API), and plain folders. A folder that is a git repository is taken as a repository: `gft get OWNER/NAME`.

## Configuration

Graft has no config file; it reads these environment variables.

| Variable | What it does | Default |
| --- | --- | --- |
| `GRAFT_HOME` | The folder where Graft keeps its notes. Wins over `XDG_DATA_HOME`. | unset |
| `XDG_DATA_HOME` | Used when `GRAFT_HOME` is unset: notes go in `$XDG_DATA_HOME/graft`. | unset, so `~/.local/share/graft` |
| `GRAFT_SANDFS` | The `sandfs` program to run (a path or a name). | `sandfs` on the `PATH` |
| `GRAFT_GITHUB_API` | GitHub API base URL, for GitHub Enterprise or a stand-in server. A trailing `/` is ignored. | `https://api.github.com` |
| `GITHUB_TOKEN` | Token that `land` uses with the API. Checked first. | unset |
| `GH_TOKEN` | Same, checked when `GITHUB_TOKEN` is unset. | unset |

Without either token, `land` runs `gh auth token` and uses what it prints; if that fails too, `land` stops with "no GitHub token". Only `land` needs a token. The token is passed to `curl` on its stdin, never on a command line.

Graft also passes its environment on to `sandfs`, `git` and `curl`, so their own settings apply. In particular `sandfs` keeps its workspaces under `$SANDFS_HOME`, else `$XDG_DATA_HOME/sandfs`, else `~/.local/share/sandfs`. Setting `XDG_DATA_HOME` therefore moves both Graft's notes and sandfs's workspaces, while `GRAFT_HOME` moves only Graft's.

The tests also read `GRAFT_REQUIRE_SANDFS` (fail instead of skipping when sandfs or a mount is missing); Graft itself doesn't.

### Where Graft keeps state

Graft's notes (copy names, the commit each sits on, pull requests, snapshots) are one file per project under the folder above:

```text
projects/OWNER/NAME.json   the project, its copies and their snapshots
projects/OWNER/NAME.lock   held while a command changes that file
projects/folder/NAME.json  a plain folder's project, named after the folder
```

The files themselves are sandfs workspaces, kept by sandfs: `NAME` for the home copy, `NAME._main` for the default branch as last fetched (the start of every new copy), `NAME.COPY` for each copy, and `NAME.COPY.N` for its snapshots. Here `NAME` is the repository's name without the owner, or the folder's name; a folder has no home or `_main` workspace, since each copy clones the folder as it is. A copy's folder sits next to the home copy as `HOME+COPY`. Short-lived files (a request body for `curl`, the pieces of a merge) go in the system's temporary folder and are removed afterwards.
