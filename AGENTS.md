# Repository Guidelines

## What this is
- Graft (`gft`, also installed as `graft`) is a small client for working in copies of a GitHub repository. Each copy is a [sandfs](https://github.com/lydakis/sandfs) folder, so nothing is cloned; GitHub keeps history, review and merging, and `gft land` makes a branch and a pull request through GitHub's API.
- It glues three things together: sandfs (the `sandfs` library crate, built in) for the files, `git` for reading commits and merging lines, `curl` for GitHub. It doesn't reimplement any of them. A feature that sandfs could offer to anyone (not just to git users) belongs in sandfs, not here.

## Layout
- `src/lib.rs`: the commands. `src/state.rs`: the project files under `$GRAFT_HOME`. `src/land.rs`: landing. `src/github.rs`: API calls. `src/tools.rs`: calling sandfs and running git.
- `tests/workflow.rs`: the whole workflow against a real sandfs mount and a stand-in GitHub. It skips without a mount, unless `GRAFT_REQUIRE_MOUNT=1` (CI sets it).

## Checks
- `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.

## Style
- Rust 2024, rustfmt settings in `rustfmt.toml`, no `unsafe`. Comments give the reason, not the mechanics.
- User-facing text is plain: say what happened to the user's files and copies, in their words.
