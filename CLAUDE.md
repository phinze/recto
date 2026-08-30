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

## Current Shape

Recto opens a jj or Git repository at its canonical root and shows the full
range from a readable base such as the branch point. The main diff has syntax
and word-level highlighting, wrapping, search, mouse and keyboard navigation,
and live reload. Files and revisions appear in optional navigator panes.

The editor handoff and workspace socket make Recto a shared review surface for
the user and a companion agent. Either side can focus code, the agent can lay
down an annotated tour, and the user can leave private agent notes or co-author
a local GitHub review draft. Public PR descriptions and conversations can be
attached as read-only context. Recto atomically saves authored state beneath
`$XDG_STATE_HOME/recto/workspaces/`, keyed by the canonical workspace root, so
standalone and Rig-launched viewers have the same restart behavior. Review
drafts are additionally keyed by repository, PR number, and head OID. Inside a
review Rig, Recto asks Rig's versioned JSON API only for PR context; `rig down`
asks Recto's public CLI to forget the workspaces whose lifecycle has ended.
Neither tool reads the other's private persistence format.

## Stack

- **ratatui 0.30** + **crossterm 0.29** — TUI framework and terminal
  backend.
- **jj / git CLIs** — invoked as subprocesses. We do *not* depend on
  `jj-lib` directly. The CLI is the supported interface; jj-lib is
  internal and changes without notice, and using it without going
  through `snapshot_workspace` is exactly the lumen trap.
- **anyhow** + **color-eyre** — error handling. anyhow in library
  code, color-eyre installed at `main` for nicer panics/reports.
- **similar** — word-level refinement inside paired removed and added lines.
- `gix` remains deliberately absent. The Git CLI is sufficient until measured
  subprocess cost says otherwise.

## Architecture

The main tension points are:

- **Backend trait.** `JjBackend` is primary; `GitBackend` is fallback.
  Detection prefers `.jj/` over a colocated `.git/`. Each backend owns a
  canonical repository root and exposes change summaries, unified diffs,
  revision history, default bases, and historical file content.
- **Base abstraction.** `Base` is an enum, jj-shaped:
  `Revision(String)` for revsets like `@-`, `trunk()`, plus a few
  named conveniences. Git's HEAD / index / working-tree / merge-base
  map onto the same enum via `Revision("HEAD")`, `Revision("@")`,
  etc., so the UI cycle logic doesn't branch on backend.
- **File navigator.** Files are grouped under directory headings, with public
  threads, shared drafts, and private notes nested under their file.
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
cargo test
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
