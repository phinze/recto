---
name: recto
description: Drive a running recto diff viewer from a companion session — scroll to and highlight a file or line span, or lay down a numbered multi-site tour whose narration advances only when the user says to continue. Load whenever you are explaining, reviewing, or walking through code changes in a workspace where recto might be open, or when asked to "show me where", "point at", or "tour" a diff. Lets you direct the user's eyes to the exact lines you're describing instead of just naming them.
---

# recto

recto is the user's jj-first terminal diff viewer. When it's open on a
workspace, any session inside that workspace can drive it: scroll the
diff to a span and highlight it, so when you say "look at the retry loop
in client.rs" you can actually put it on screen.

Discovery is automatic. recto listens on a Unix socket keyed to the
workspace root, so `recto focus …` run from anywhere inside the repo
reaches the recto reviewing it. No env var, no socket path to thread
through.

## Inside a multi-repo Rig

A Rig keeps one persistent recto per repo and shows the currently relevant one
beside its task-level agent. When `RIG_BASEDIR` is set, route every command
through Rig and name the repo subdirectory from the generated rig instructions:

    rig recto cloud ping
    rig recto cloud focus src/app.rs:42-58
    rig recto brand annotate 'docs/index.md:10=Update the headline'
    rig recto cloud clear

`rig recto <repo>` promotes that repo's existing viewer into the main window;
any remaining arguments are forwarded to recto from the correct workspace.
The command is idempotent when that repo is already visible and preserves
recto's exit codes. Use this form even when you think the right viewer is
already active, so the user's visible surface and the command target cannot
drift apart. Do not run tmux pane commands yourself, and do not use `recto -R`
as a substitute: Rig owns the carousel and must know which viewer to surface.

Outside a Rig, use the ordinary `recto …` commands below.

## Commands

    recto focus PATH:START-END   # highlight lines START..END
    recto focus PATH:LINE        # highlight a single line
    recto focus PATH             # scroll to the file, no line span
    recto annotate SPEC [SPEC…]  # label multiple spans as numbered steps
    recto clear                  # remove the highlight and any annotations
    recto ping                   # is a recto listening here?

PATH is relative to anywhere in the workspace (recto normalizes it to
the repo root). Line numbers are **new-side** — the line numbers in the
post-change file, the ones you'd see in your editor after the edit.

Annotate SPECs are `PATH:LINE=label` or `PATH:START-END=label`. Argument
order sets the step numbers, and each call replaces the whole set.

## Choose the active surface

Start with `recto ping`. Its `surface` and `capabilities` fields are the
command contract for the view the user is actually looking at:

- `surface: "recto"`: `focus` and `annotate` land immediately, but only on
  spans rendered in the current diff. Use `files` to check candidate paths.
- `surface: "neovim"`: `focus` lands immediately and may target any file in
  the workspace, whether or not it appears in `files`. `annotate` is deferred
  until the user returns to recto and remains limited to the current diff.
- `surface: "editor"`: recto cannot drive this editor. Both commands are
  deferred until the user returns and are limited to the current diff.

Choose the surface from the requested kind of tour. A request to explain how a
subsystem works is a code tour, so prefer live neovim focus when available. A
request to review the current change is a diff tour, so use recto annotations.
Do not ask the user to change the diff base merely to show current source that
neovim can already open.

## Pace tours one span at a time

Treat every interactive tour as user-paced. Make showing a span and explaining
it one complete assistant turn:

1. `focus` exactly one span.
2. Explain only that span and its immediate role.
3. End the response and wait for the user to say `next`, ask a question, or
   otherwise explicitly continue.

Never issue a second `focus` call in the same turn, including in a batch of tool
calls. Keep the highlight in place while answering a question about the current
span. Treat silence as a stop, not permission to advance. Treat a request such
as "walk me through this" as authorization to start at the first span, not to
run the whole tour without stopping. Skip these pauses only when the user
explicitly asks for an uninterrupted or all-at-once walkthrough.

Treat `annotate` as the exception to the one-span command limit because it lays
down a standing map rather than moving the active pointer. Annotate all tour
sites up front if useful, but explain only step 1 and then wait. On each later
turn, `focus` the one step being discussed and stop again. Do not clear the
final highlight until the user acknowledges that the tour is done or asks to
leave the tour.

## How to use it in a tour

For a single-threaded walkthrough, `focus` the current span, describe it, and
yield the turn before advancing. This works in both recto and a live neovim,
subject to the scope reported by `capabilities.focus`. The highlight is sticky:
it stays put while the user reads or asks questions, until a later turn focuses
something else or calls `recto clear`. Re-focusing the same span re-fires its
attention flash, so it is never a no-op. Once the user is finished with the
tour, use `recto clear` so you do not leave a stray highlight.

For a multi-site tour — "step 1 here, step 2 there" — lay all the steps
down first:

    recto annotate 'src/parser.rs:42-58=Step 1: parse the manifest' \
                   'src/link.rs:30=Step 2: the new request variant'

Each step renders as a numbered note row woven into the diff above its
span. recto scrolls to step 1 immediately; the user jumps between steps
with the `1`–`9` keys, so prefer at most nine. The annotations stay up
while you talk, and you can still `focus` individual spans on top of
them as the conversation moves — focus is the bright "look here now"
pointer, annotations are the standing map. `recto clear` (or the user
pressing Esc) removes the whole set.

## Reading the exit code

On the recto surface, recto is passive: it will not switch the diff base to
find your target. It reports rather than chases. Live neovim focus is the
exception: its scope is the workspace, not the current diff.

- **exit 0** — landed. For `annotate`, a note on stderr may list sites
  that didn't resolve; the rest are on screen.
- **exit 1** — recto is running but refused. Usually "not in current
  diff" (the file isn't in the diff for the base recto currently shows)
  or "outside any shown hunk" (the file is in the diff but those lines
  aren't part of a changed hunk). For `annotate` this means *no* site
  resolved. If you expected the file to be there, the user may need to
  cycle the base with `b`; tell them, don't retry blindly.
- **exit 2** — no recto is listening for this workspace (or you're not
  in a repo). Don't keep trying; just describe the change in text.

## Reading the ping

Always `recto ping` first if you're unsure recto is open. A clean ping
(exit 0) means the focus calls will land. But ping is more than a
heartbeat: on success it prints a JSON status object on stdout (human
notes and errors stay on stderr, so you can read one without the other).
It looks like:

    {
      "version": "0.1.0",
      "pid": 3107686,
      "backend": "jj",
      "workspace_root": "/home/me/src/recto",
      "base": "@-",
      "scope": "range",
      "files": ["src/backend.rs", "src/link.rs", "src/main.rs"],
      "surface": "recto",
      "capabilities": {
        "focus": {"delivery": "live", "scope": "current_diff"},
        "annotate": {"delivery": "live", "scope": "current_diff"}
      },
      "focus": false,
      "annotations": 0
    }

The `files` array is always the changed-path list in the diff recto currently
shows. It bounds commands only when their capability scope is `current_diff`.
When focus scope is `workspace`, a path absent from `files` is still a valid
focus target. `scope` is `"range"` for the whole base diff or `"rev"` when
narrowed to a single revision.
