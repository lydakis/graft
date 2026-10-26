# Graft

Work in instant copies of a GitHub repository. Each copy is a [sandfs](https://github.com/lydakis/sandfs) folder over a commit of the default branch, so nothing is cloned, a copy appears in a fraction of a second, and it costs only what it changes. When a copy is good, `gft land` turns it into a branch and a pull request through GitHub's API, with no local commit and no push.

The command is `gft` (also installed as `graft`).

```bash
gft get lydakis/sandfs              # ~/code/sandfs: the home copy, tracking main, nothing downloaded up front
cd sandfs
gft new fix-unmount                 # ../sandfs+fix-unmount, on the newest commit of main
cd ../sandfs+fix-unmount
# edit, build, test as in any folder; git works here too, but nothing needs staging or committing
gft changes                         # A/M/D for each file this copy changed
gft save "busy unmount works"       # a snapshot
gft undo                            # back to the last snapshot
gft sync                            # main moved on: move onto it, merging files changed on both sides
gft land -m "Recover from a busy unmount"   # branch graft/fix-unmount and a pull request
gft land                            # after review fixes: one more commit on the same pull request
gft rm fix-unmount
```

Many agents at once:

```bash
gft new agent-{01..20}              # twenty copies
gft ls                              # files changed per copy, and which copies changed the same files
gft changes agent-07 agent-03       # how two attempts differ
gft new agent-07b --from agent-07   # branch an attempt mid-task, keeping its changes
gft land agent-07 --draft
gft rm agent-{01..20} agent-07b
```

## Commands

| Command | What it does |
| --- | --- |
| `gft get OWNER/NAME [DIR]` | Mount the default branch at DIR (default `./NAME`) as the home copy. |
| `gft new NAME... [--from COPY]` | Copies on the newest commit (fetched once for all of them), or on another copy with its changes. They appear next to the home copy as `HOME+NAME`. |
| `gft ls` | Copies, files changed, the commit each sits on, pull requests, overlaps. |
| `gft path NAME` | A copy's folder. |
| `gft changes [A] [B]` | What copy A changed, or how A differs from B. |
| `gft save [NOTE]` / `gft undo [N]` | Snapshot the copy; go back N snapshots. |
| `gft sync [NAME]` | Move the copy onto the newest commit. Files changed on both sides are merged line by line with `git merge-file`; conflicts are left marked in the file and listed. |
| `gft land [NAME] [-m TITLE] [--draft]` | Commit the copy's changes on branch `graft/NAME` on GitHub and open a pull request (against the copy it came from, if that one landed). Landing again adds a commit. |
| `gft rm NAME...` | Delete copies and their snapshots. Pull requests stay. |

Commands that take a copy use the one the current folder is in when none is named.

## Needs

- A `sandfs` that has `sandfs update` (lydakis/sandfs#156; until it ships in a release, `cargo install --git https://github.com/lydakis/sandfs sandfs-cli`), and what it needs to mount: FUSE 3 on Linux, nothing on macOS.
- `git` and `curl`.
- A GitHub token for `land`: `GITHUB_TOKEN` or `GH_TOKEN`, else whatever `gh auth token` gives.

## Limits

- `sync`, `undo` and `rm` unmount the copy for a moment, so no program may be working inside it then (an agent's shell sitting in the folder counts). Moving a mounted copy in place is planned in sandfs.
- Landing uploads each changed file as one API call. A few hundred changed files take a while, and GitHub allows 5,000 calls an hour per token.
- GitHub only, for now. Plain folders that aren't git repositories are next: there `land` will write the copy's changes back into the folder.

Graft keeps its own notes (copy names, the commit each sits on, pull requests, snapshots) under `$GRAFT_HOME`, by default `~/.local/share/graft`. The files themselves are sandfs workspaces named `REPO.COPY`.
