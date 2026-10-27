# Graft

Parallel agents on big repositories. Give each agent its own copy of a git repository, made in a fraction of a second with nothing cloned and no disk taken until it changes something, and let each land its own branch and pull request. Graft is a client for your git host, not a replacement for git: review, history and merging stay on GitHub, GitLab or whatever server holds the repository, and git keeps working inside every copy.

Each copy is a [sandfs](https://github.com/lydakis/sandfs) folder over a commit of the default branch: the copies share one set of the repository's files, downloaded from the remote as they are first read, and each copy keeps only what it changes. Nothing is cloned up front, so the first read of a file, and the first search or build across the whole tree, waits for those files to arrive; after that they are on disk and shared by every copy. How that compares with git worktrees, measured: [docs/compared-with-worktrees.md](docs/compared-with-worktrees.md). When a copy is good, `gft land` turns it into one commit on its own branch, pushed with plain `git push`, and on GitHub or GitLab opens a pull request (merge request) for it. Any remote git can push to works; the `gh` and `glab` commands are never needed.

The command is `gft` (also installed as `graft`). For plain folders, see [Plain folders](#plain-folders).

```bash
gft get lydakis/sandfs              # ./sandfs: the home copy, tracking main, nothing downloaded up front
cd sandfs
gft new fix-unmount                 # ../sandfs+fix-unmount, on the newest commit of main
cd ../sandfs+fix-unmount
# edit, build, test as in any folder; git works here too, but nothing needs staging or committing
gft changes                         # A/M/D for each file this copy changed
gft changes --count                 # just the totals: 3 added, 5 modified, 1 deleted
gft diff                            # the changes line by line, as `land` would send them
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
gft changes agent-07 agent-03       # which files two attempts differ in
gft diff agent-03 agent-07          # and how, line by line
gft new agent-07b --from agent-07   # another attempt from agent-07's files so far
gft land agent-07 --draft
gft ls --json                       # the same for a program: copies, files, pull requests, overlaps
gft rm agent-{01..20} agent-07b       # refuses copies with changes that never landed
gft rm --force agent-07b            # delete one anyway, losing those changes
```

## Commands

| Command | What it does |
| --- | --- |
| `gft get REPO [DIR] [--forge KIND[:PROJECT]]` | Mount the default branch at DIR (default `./NAME`) as the home copy. REPO is OWNER/NAME on GitHub, or the URL of any repository git can fetch and push (`https://`, `ssh://`, `git@host:path`, `file://`). Copies land on that remote. Repositories on github.com and gitlab.com also get pull requests and merge requests; for a server of your own, `--forge github` or `--forge gitlab` says which kind it is (its API at the same host), with `:GROUP/NAME` when the project's path differs from the URL's, and `--forge none` lands branches alone. See [Remotes and forges](#remotes-and-forges). Given a git checkout on this machine (`gft get .`), the checkout is the home copy: copies start from its commit and its uncommitted changes, `sync` follows its branch, and copies land on its `origin`, along with any commit of the checkout's not pushed there yet. Given a plain folder (`./notes`, `/data/survey`), the folder itself is the home copy (see [Plain folders](#plain-folders)). |
| `gft new NAME... [--from COPY \| --on COPY]` | Copies on the newest commit (fetched once for all of them, and not again for 10 seconds, so many `gft new` at once share one fetch), or on another copy with its changes so far. `--from` makes another attempt at the same work: its pull request goes against the default branch with all the changes. `--from HOME` (the home copy's folder name) starts from files changed in the home copy. `--on` builds on top of a copy that landed: its pull request goes against that copy's branch and shows only its own changes. They appear next to the home copy as `HOME+NAME`. |
| `gft ls [--json]` | Copies, files changed, the commit each sits on, pull requests, and overlaps: other copies that changed the same file differently (the same bytes in both is no overlap). A copy whose folder is empty after a restart says so. Outside a copy, every project's. |
| `gft status [--json]` | In a copy: its name, folder, commit, files changed, conflicts, snapshots and pull request. In the home copy: the project, its commit, how many copies exist, and any files changed in the home copy itself. |
| `gft path NAME` | A copy's folder. |
| `gft changes [A] [B] [--count \| --json]` | What copy A changed, or how A differs from B (also when they sit on different commits). `--count` prints only the totals, like `3 added, 5 modified, 1 deleted`. |
| `gft diff [A] [B]` | The copy's changes line by line, as `land` would send them; given two copies, what changes from A to B. |
| `gft up [NAME...]` | Mount copies again after a restart or an unmount left their folders empty: those named, else every copy of the project and its home copy. Other commands mount the copy they work on themselves, and say so. |
| `gft save [NOTE]` / `gft undo [N]` | Snapshot the copy; go back N snapshots (default 1). Undo first saves the copy as it is (unless its newest snapshot has it already) and keeps newer snapshots, so `gft undo` again returns to how it was. Both take `-c COPY`. |
| `gft undo --list` | The copy's snapshots, newest first, each with its number for `undo N`, how long ago it was taken and its note. Changes nothing. |
| `gft sync [NAME]` | Move the copy onto the newest commit, in place: programs working in the copy carry on. Files changed on both sides are merged line by line with `git merge-file`, into the file's new name when upstream renamed it; conflicts are left marked in the file and listed, `status` shows them until they are settled, and `sync` exits with 2 while any are left. Commits made with git in the copy are set aside first (`git reset --soft`): their changes stay in the files, and `git reflog` keeps them. A snapshot is saved before anything changes, so `gft undo` goes back to before the sync. Run in the home copy, it moves the home copy onto the newest commit the same way. After the copy's pull request, or the one it was built on with `new --on`, is merged, upstream's version is merged from what that pull request landed, so the merged work itself never conflicts; the copy then builds on the default branch, and lands next as a new pull request. |
| `gft land [NAME] [-m TITLE] [--body TEXT \| --body-file PATH] [--draft] [--force] [--dry-run] [--new-pr] [--allow-conflicts]` | Commit the copy's changes, push them as branch `graft/NAME`, and open a pull request, or a merge request on GitLab (against the branch of the copy it was built on, with `new --on`). With no forge, the branch is all: `land` prints it, and any link the server offers for opening a review. Landing again adds a commit, also when the copy was put back as its base has it, which empties the pull request. The description is TEXT or the contents of PATH (`-` reads standard input), followed by a line saying it was landed with `gft land`. Giving one when landing again replaces the description on the open pull request; giving none leaves it as it is. A landing cut short (a lost connection, the forge refusing the pull request) is finished by landing again, without a second commit or pull request. If the branch on the remote has commits the copy didn't land, `land` stops rather than undo them; `--force` lands anyway. A first landing whose `graft/NAME` exists already (an earlier copy's of the same name) goes to `graft/NAME-2` instead, with none of that branch's history. It stops while files still have conflict markers from `sync`, unless `--allow-conflicts`. A pull request that was merged takes no more commits: after `sync`, the copy lands as a new one. Without a forge, a branch counts as merged once the default branch has every file as the landing left it. One closed without being merged stops `land`: reopen it, or `--new-pr` opens another. A pull request built `--on` work merged since goes against the default branch from the next landing. `--dry-run` (`-n`) prints what landing would do (the branch, the commit's parents, the pull request and the files) and changes nothing. |
| `gft rm [--force] [--delete-branch] NAME...` | Delete copies and their snapshots. Pull requests stay; `--delete-branch` also deletes each copy's branch on the remote once its pull request is merged or closed (with no forge, any time), unless the branch has commits the copy didn't land. A copy with changes that never landed (it never landed, or files changed since its last landing, put back as the base has them included) is kept, and the files that would be lost are counted, unless `--force` is given. |
| `gft drop [--force] [PROJECT]` | Remove a whole project (OWNER/NAME, or the home copy's folder name; default the one here): every copy, as `rm` would, the home copy's mount and what Graft keeps, so `gft get` can start over. A plain folder or a checkout stays as it is. |

Commands that take a copy use the one the current folder is in when none is named. `sync` exits with 2 while conflicts are left, and `land` refuses them; other errors exit with 1.

### The home copy

The home copy (`./NAME` after `gft get`) is where copies come from, not where work happens: it follows the default branch, and nothing in it lands. Files changed there show in `gft status`, which says so, and only reach a copy made with `gft new NAME --from HOME`. `gft sync` there moves it onto the newest commit; `gft drop` refuses to remove it while it has changes, unless `--force`.

## Many agents at once

Give each agent its own copy, named for its task, and let each land its own pull request. No change is shared between copies, so one agent's half-finished edit never breaks another's build, and a new copy takes no disk until it changes something.

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
gft land fix-timeout --draft
gft new retry-docs --on fix-timeout

gft sync add-retries                       # main moved on
gft land add-retries -m "Retry on timeout"             # one pull request per copy
```

- **Check overlaps with `gft ls`.** Each copy's line ends with `overlaps OTHER on FILE` for every other copy that changed a file it also changed. It only sees changes made so far, so an agent runs it before it starts on a shared file and once more before it lands. `gft ls` lists every copy of the repository, so an agent reads its own line. An overlap isn't an error: either the agents split the work differently, or the later one expects to resolve a merge. `gft changes OTHER` lists the files the other copy changed, and `gft changes MINE OTHER` how the two differ.
- **Stack dependent work with `gft new NAME --on COPY`.** The new copy starts with that copy's changes, so the second agent builds on the first one's work without waiting for it to merge. The first copy must have landed (a draft will do); the stacked one's pull request goes against its branch. To try the same work another way instead, `--from COPY` starts from its files with a pull request of its own against main. Independent work always starts from main (plain `gft new`), so it never inherits another agent's unfinished work.
- **Sync when main has moved and you are about to land, or when you need what landed.** Other agents' pull requests move main; a copy stays on the commit it started from until it syncs. Syncing is not needed to keep working, and an agent can sync from inside its copy while its programs keep running there. Files changed on both sides are merged line by line; any listed as conflicts have `<<<<<<<` markers to fix before you build, test and land, and `sync` exits with 2 until they are fixed.
- **Land each copy when it is good.** `gft land -m TITLE` opens `graft/NAME` as a pull request; `--draft` leaves it for review. A copy made `--on` another opens its pull request against that one's branch and shows only its own changes, even if the other lands more later; review them in that order. Landing again after review comments adds a commit to the same pull request. When the pull requests are merged, `gft rm` the copies.

## When plain git is better

Graft pays off when many agents (or people) work on one big repository at once, each on its own small change. It costs something everywhere else, and plain git is better for:

- **One person on one branch.** A clone or a worktree is a folder like any other, with nothing in between; a copy adds a mount, and its first read of each file waits for the remote.
- **Full-tree builds and searches.** The first build or search across the whole tree in a copy downloads every file it reads; after that they are shared by every copy, but a clone has them from the start.
- **Working offline.** Files nobody has read yet come from the remote, and `land` and `sync` need it.
- **Local history.** A copy starts with one commit. `git fetch --deepen=N origin` brings history into it when wanted, but work that lives in `git log`, `git blame` and `git bisect` all day is better in a clone.
- **Signed commits and commit hooks.** `land` makes its own commits, unsigned and without your hooks (see [Git in a copy](#git-in-a-copy-and-what-land-sends)).

Measured against worktrees: [docs/compared-with-worktrees.md](docs/compared-with-worktrees.md).

## Plain folders

A folder that isn't a git repository works the same way, with the folder itself as the home copy and no remote:

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
- **`land` writes the copy's changes into the folder**: changed and new files, and deletions. If the folder changed one of those files since the copy was made or last synced, nothing is written and the files are named; `gft sync` merges the folder's version in, then `land` again. After landing, the copy sits on the folder as it is now, with no changes of its own.
- **`sync` merges what changed on both sides**, line by line, against the folder as the copy took it, like a repository's sync: conflicts are marked in the file, and `land` refuses them until they are settled.
- `save`, `undo`, `ls`, `changes` and `rm` work as for a repository. `land` takes no `-m`, `--body`, `--draft` or `--force`.

## Install

Each [release](https://github.com/lydakis/graft/releases) has `gft` (and `graft`) for macOS (Apple silicon, Intel) and Linux (x86_64, aarch64; static, any distribution). On an Apple silicon Mac:

```bash
curl -L https://github.com/lydakis/graft/releases/latest/download/graft-aarch64-apple-darwin.tar.gz | tar xz
mv graft-aarch64-apple-darwin/gft graft-aarch64-apple-darwin/graft ~/.local/bin/   # or any folder on your PATH
rm -r graft-aarch64-apple-darwin
```

Other machines: `graft-x86_64-apple-darwin`, `graft-x86_64-unknown-linux-musl`, `graft-aarch64-unknown-linux-musl`. From source: `cargo install --locked --git https://github.com/lydakis/graft`.

sandfs is built into `gft`, so it needn't be installed.

## Needs

- What a mount needs: FUSE 3 on Linux, nothing on macOS.
- `git`, and `curl` for a forge's API.
- To land, push access to the remote, as `git push` would have it (your SSH keys or credential helper), and for pull requests a token for the forge (see [Remotes and forges](#remotes-and-forges)).

## Limits

- `undo` back past a `sync` makes the copy again, unmounting it for a moment, so no program may be working inside it then (an agent's shell sitting in the folder counts). So does `rm`. Other undos, and `sync`, change the copy in place and need nothing stopped.
- A copy starts with the default branch's newest commit and no older history, so `git log` and `git blame` show only that commit until asked for more: `git fetch --deepen=100 origin` in the copy brings the last hundred commits, `git fetch --unshallow origin` all of them. Either fetches commits, not file contents, which still come as they are read.
- Repositories on any git remote, checkouts of them, and plain folders. Pull requests on GitHub (and GitHub Enterprise) and merge requests on GitLab (and self-managed GitLab); other forges get branches only, for now.

## Git in a copy, and what `land` sends

A copy is a git checkout as far as git can tell: `git status`, `diff`, `log`, `blame` and local commits work in it, and editors and build tools see an ordinary folder. `land` doesn't use git's commits, though. It reads the copy's files as they are on disk, builds one commit from them on the base (in a separate index, so the copy's own index and `HEAD` are left alone), and pushes that commit to the branch, which draws a line around what git would otherwise do for you:

- **Files go as their bytes.** A changed file is sent as it is on disk, without the repository's clean filters or line-ending conversion (`.gitattributes` `filter`, `text`, `eol`), as `git hash-object --no-filters` would see it. Files come into a copy the same way, as stored in the repository.
- **Git LFS** files are their pointer files in a copy. A pointer left alone lands unchanged; a real file written in its place lands as a plain git file, not through LFS.
- **No local hooks run.** The commit is built with git's plumbing and pushed with `--no-verify`, so `pre-commit`, `commit-msg`, `pre-push` and the like never run; run your checks before landing. The server's own hooks and branch rules apply as to any push.
- **Commits aren't signed**, so a branch rule that requires signed commits refuses them.
- **Local commits are ignored.** `land` sends what the files are now, in one commit per landing, with the message from `-m`; commits made with git inside the copy, and their messages, don't reach the remote. `sync` sets them aside, keeping their changes in the files.
- **Submodules** don't appear in a copy, and a copy's landing leaves them as they are.
- Modes: files, executable files and symbolic links land as such.

## Configuration

Graft has no config file; it reads these environment variables.

| Variable | What it does | Default |
| --- | --- | --- |
| `GRAFT_HOME` | The folder where Graft keeps its notes. Wins over `XDG_DATA_HOME`. | unset |
| `XDG_DATA_HOME` | Used when `GRAFT_HOME` is unset: notes go in `$XDG_DATA_HOME/graft`. | unset, so `~/.local/share/graft` |
| `GRAFT_GITHUB_API` | GitHub API base URL, for a stand-in server. A trailing `/` is ignored. | `https://api.github.com` on github.com, else `https://HOST/api/v3` |
| `GRAFT_GITLAB_API` | GitLab API base URL, likewise. | `https://HOST/api/v4` |
| `GITHUB_TOKEN` | Token for GitHub's API. Checked first. | unset |
| `GH_TOKEN` | Same, checked when `GITHUB_TOKEN` is unset. | unset |
| `GITLAB_TOKEN` | Token for GitLab's API. | unset |

sandfs's own settings apply too, and Graft passes its environment on to `git` and `curl`. In particular sandfs keeps its workspaces under `$SANDFS_HOME`, else `$XDG_DATA_HOME/sandfs`, else `~/.local/share/sandfs`, the same ones the `sandfs` command shows if it is installed. Setting `XDG_DATA_HOME` therefore moves both Graft's notes and sandfs's workspaces, while `GRAFT_HOME` moves only Graft's. Mounts are served by a process that stays after `gft` returns: `gft` itself, run again as `gft sandfs daemon`.

The tests also read `GRAFT_REQUIRE_MOUNT` (fail instead of skipping when a mount can't be made); Graft itself doesn't.

## Remotes and forges

A copy lands with plain git: one commit, pushed to `graft/NAME` on the repository's remote with a lease (`--force-with-lease`), so a branch someone else moved is never overwritten. Anything git can push to works, with the credentials git already uses there: SSH keys, a credential helper, a token in the URL. A remote with no forge gets branches, and nothing else is needed.

A forge adds the review: a pull request on GitHub, a merge request on GitLab, opened, retargeted and described by `land`, and asked by `sync` whether it was merged. Graft knows github.com and gitlab.com by name. For a server of your own, say which kind it is when getting the repository:

```bash
gft get https://git.example.com/team/app.git --forge gitlab          # self-managed GitLab: API at https://git.example.com/api/v4
gft get git@ghe.example.com:org/app.git --forge github               # GitHub Enterprise: API at https://ghe.example.com/api/v3
gft get https://git.example.com/team/sub/app.git --forge gitlab:team/sub/app
gft get ssh://git@code.example.com/app.git                            # no forge: branches only
gft get https://github.com/org/app --forge none                      # GitHub, but branches only
```

The forge's API needs a token: `GITHUB_TOKEN` or `GH_TOKEN` for GitHub, `GITLAB_TOKEN` for GitLab. Without one, Graft asks `gh auth token` or `glab config get token` for that host, if they are installed; if nothing gives a token, `land` stops before pushing and says how to sign in. The same token is offered to `git push` when the remote is an `https://` URL that git has no credentials for, so a token alone is enough for both. Tokens reach `curl` and `git` through their standard input and environment, never a command line. Only `land`, `sync` after a landing, and `rm --delete-branch` talk to the forge.

### Where Graft keeps state

Graft's notes (copy names, the commit each sits on, pull requests, snapshots) are one file per project under the folder above:

```text
projects/OWNER/NAME.json   the project, its copies and their snapshots
projects/OWNER/NAME.lock   held while a command changes that file
projects/folder/NAME.json  a plain folder's project, named after the folder
```

The files themselves are sandfs workspaces, kept by sandfs: `NAME` for the home copy, `NAME._main` for the default branch as last fetched (the start of every new copy), `NAME.COPY` for each copy, and `NAME.COPY.N` for its snapshots. Here `NAME` is the repository's name without the owner, or the folder's name; a folder has no home or `_main` workspace, since each copy clones the folder as it is. A copy's folder sits next to the home copy as `HOME+COPY`. Short-lived files (a request body for `curl`, the pieces of a merge) go in the system's temporary folder and are removed afterwards.
