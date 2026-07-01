# recto

A jj-first terminal diff viewer. The right-hand page where the new
text lives — and where you read your agent's work.

## Why This Exists

Every terminal diff tool worth naming is git-shaped. `delta`,
`difftastic`, `nvim-diffview` — designed around git refs and a git
working tree. `lumen` and `hunk` ship "jj support" but treat it as a
backend afterthought, and the seams show: lumen's jj-lib path skips
`snapshot_workspace`, so working-copy edits never reach `@` and watch
mode happily re-renders the empty diff.

recto inverts that. jj is the primary model; git is the fallback. The
data layer is shaped around jj's revsets, working-copy snapshot
semantics, and operation log — and a git repo is just the special case
where the available revisions are HEAD, index, working tree, and
merge-base.

## What This Project Does

A TUI for reviewing diffs the way you'd review a GitHub PR: unified
diffs in a main pane, a directory tree of changed files on the left,
and a workflow built around the cadence of reviewing agent-authored
changes.

The originating sketch lives in memex:
`~/src/github.com/phinze/memex/Projects/Ideas/review-first-diff-tool.md`.
That doc is the source of truth for *why* this exists; this file is
the source of truth for *how it's built*.

## v0 Scope

Smallest thing that proves the architecture:

1. Open a jj (or git) repo, show a left-pane file tree of changes
   against `@-` (jj) or merge-base (git). Right pane: concatenated
   unified diffs.
2. `j/k` to scroll, `tab` to focus the tree, `enter` to jump to a
   file's diff in the main pane.
3. `b` to cycle base. In jj: `@-` → `trunk()` → `@--` → … In git:
   working tree → merge-base → HEAD → index. Header shows the current
   revision/base.
4. `e` to open the file under cursor at the right line in `$EDITOR`,
   come back cleanly.

Explicit non-goals for v0: watch mode, search, syntax highlighting,
tmux/agent integration. Those layer in once the v0 loop feels right.

## Stack

- **ratatui 0.30** + **crossterm 0.29** — TUI framework and terminal
  backend.
- **jj / git CLIs** — invoked as subprocesses. We do *not* depend on
  `jj-lib` directly. The CLI is the supported interface; jj-lib is
  internal and changes without notice, and using it without going
  through `snapshot_workspace` is exactly the lumen trap.
- **anyhow** + **color-eyre** — error handling. anyhow in library
  code, color-eyre installed at `main` for nicer panics/reports.
- `gix` and `similar` deliberately not pulled in for v0. We can swap
  the git backend to gix later if subprocess perf becomes a problem,
  and `similar` for richer intra-line diffing only when the rendering
  shape asks for it.

## Architecture

The shape will emerge as we build, but the tension points we already
know:

- **Backend trait.** `JjBackend` is primary; `GitBackend` is fallback.
  Detection: `.jj/` present → jj (regardless of `.git/` colocation,
  since on colocated repos jj is the source of truth for working-copy
  state). `.git/` only → git. Each backend exposes:
  `list_changes(base) -> Vec<FileChange>`,
  `unified_diff(base, path) -> String`,
  `available_bases() -> Vec<Base>`.
- **Base abstraction.** `Base` is an enum, jj-shaped:
  `Revision(String)` for revsets like `@-`, `trunk()`, plus a few
  named conveniences. Git's HEAD / index / working-tree / merge-base
  map onto the same enum via `Revision("HEAD")`, `Revision("@")`,
  etc., so the UI cycle logic doesn't branch on backend.
- **Tree pane.** Real tree with collapse/expand, not a flat list.
  Build a `FileTree` from path components; render hand-rolled at first
  (avoid `ratatui-tree-widget` until we know we need it).
- **Editor handoff.** `disable_raw_mode` + leave alternate screen,
  spawn `$EDITOR +<line> <path>`, re-enter on return. SIGWINCH
  between handoff and return needs handling.

## Dogfooding

This repo is itself a jj workspace. recto reviews its own diffs. If
the v0 loop doesn't feel good for this codebase's own development, we
have our answer.

## Development

```sh
# direnv loads the flake on cd; first time, run: direnv allow
cargo build
cargo run

# Format / lint
cargo fmt
cargo clippy -- -D warnings
```

## Conventions

- Fish shell, nix devShell, jj for VCS on this machine.
- `cargo fmt` before commit. `cargo clippy` clean.
- Comments only when the *why* is non-obvious. Doc comments on public
  API; inline comments rare.
- One module per pane / concern as the surface grows.

## Iteration Style

Tiny slices, snapshotted as they land. Each slice is one jj rev:

- Before starting, name two or three candidates and recommend one.
  Build when Paul gives the green light.
- Set the rev description before editing (`jj desc -m "..."`) and
  refine as scope clarifies.
- Green before snapshot: `cargo fmt` + `cargo clippy -- -D warnings`
  clean.
- `jj new` between slices, leave them stacked — Paul squashes to taste,
  Claude doesn't squash into prior revs.
- End each slice with a "fire it up" note: what to try, what to watch
  for.

Resist premature abstraction even more aggressively than the v0 spec
suggests. Trait methods, helpers, and config knobs get added when a
caller arrives, not before — `-D warnings` enforces it.

## Shipping a Revision

This is a personal, single-author repo: work lands straight on `main`, no
PR ceremony, `pr-time` is overkill. Shipping is a touch heavier than rig
only because recto is consumed downstream by nix-config as a flake input
(`recto.url = "github:phinze/recto"` in its `flake.nix`), which pins a
specific commit in `flake.lock`. The companion-session skill at
`skills/recto/SKILL.md` ships from the same input, so binary and skill
always land together. To ship a rev:

1. Advance `main` and push: `jj bookmark move main --to @ && jj git
   push --bookmark main`. After this the rev is no longer reorganizable.
2. In nix-config: `nix flake update recto`.
3. `nh os switch` (or the host equivalent) to build and activate. If
   that goes green, commit the nix-config lock bump and push.
