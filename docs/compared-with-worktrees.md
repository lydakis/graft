# Graft copies compared with git worktrees

Measured on 2026-10-26 on Linux (4 cores, FUSE, ext4), with `gft` built from this repository, against the same repository checked out as a clone with worktrees. Times are wall-clock seconds from one run; disk is what `du` reports.

The repository is [lydakis/sandfs](https://github.com/lydakis/sandfs): 88 MB as a full clone (mostly history), about 2 MB of files per checkout, and about 850 MB of build output after `cargo build -p sandfs-cli && cargo test -p sandfs-core`, which is the build and test step below. Crates were already downloaded on both sides.

## Cold: nothing on this machine yet

| | git clone | Graft |
| --- | ---: | ---: |
| Get the repository and a first attempt | 1.4 | 1.7 |
| First search over the whole tree (`rg`) | 0.01 | 0.60 |
| First build and test | 33.6 | 40.3 |
| Disk afterwards | 928 MB | 919 MB |

## Warm: repository here, one build done

| | worktree | Graft copy |
| --- | ---: | ---: |
| New attempt | 0.03 | 1.26 |
| Search in it | 0.01 | 0.03 |
| Build and test in it | 33.7 | 40.2 |
| Disk for the attempt before building | 2 MB | 24 KB |
| Disk for the attempt after building | 925 MB | 919 MB |
| Branch unfinished work (3 changed files, one new) | 0.05 | 0.09 |
| Ten new attempts | 0.28 | 0.88 |
| Disk for ten attempts before building | 19.8 MB | 240 KB |

Making a worktree for unfinished work means a new worktree, `git diff | git apply`, and copying new files by hand; `gft new NAME --from COPY` does it in one step.

## What this says

- On a small repository a worktree is faster to make. `gft new` spends most of its 1.26 s asking GitHub for the newest commit of the default branch, which every `new` does.
- A copy takes almost no disk until something writes to it. A worktree takes a full checkout up front: 2 MB here, 365 to 606 MB for VS Code (sandfs measurements, 2026-10-21).
- Build output costs the same either way. Each attempt builds into its own folder, so ten built attempts take ten builds' worth of disk with either tool.
- Building inside a copy took about 20% longer here, since every file goes through the mount.
- The first search over a whole copy is slower because files arrive from GitHub as they're first read. After that, searches cost about the same as on disk.

The advantage grows with the repository. On VS Code (sandfs measurements, 2026-10-21, Linux):

| | git | sandfs |
| --- | ---: | ---: |
| Get the repository | 33 s, 786 MB (blobless clone) | 1 s |
| New attempt | 3.1 s, 365 MB (worktree) | 0.01 s |
| First whole-tree search | 0.1 s | 7 s |

## Not measured here

- **Time to a real pull request.** `gft land` makes 4 calls to GitHub's API plus one per changed file, then one more to open the pull request. A worktree needs `git commit`, `git push` and `gh pr create`. The test suite checks landing against a stand-in for the API, not GitHub itself.
- **Recovery after an interrupted landing.** Running `gft land` again finishes it without a second commit or pull request (see the tests). With git, you run `git push` or `gh pr create` again.
