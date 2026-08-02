---
name: recto
description: Drive a running recto diff viewer from a companion session — scroll to and highlight a file or line span, or lay down a numbered multi-site tour whose narration advances only when the user says to continue. Load whenever you are explaining, reviewing, or walking through code changes in a workspace where recto might be open, or when asked to "show me where", "point at", or "tour" a diff. Lets you direct the user's eyes to the exact lines you're describing instead of just naming them. Also runs the other way: collects the review comments the user left on the diff, so load it whenever they say they left notes, ask you to address their comments, or mention having marked something up.
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
    rig recto cloud comments
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
    recto comments               # collect the review comments the user left

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

## Reading the user's review comments

Everything above points the user's eyes at code. `recto comments` runs the
other way: it hands you the notes they left while reading the diff, each
one anchored to a span and quoted with the surrounding lines. They write
those by moving the cursor with `j`/`k` and pressing `c` in recto, which is
worth mentioning if they ask how to give you line-level feedback.

    recto comments

The output is markdown on stdout, one numbered section per note, with the
commented lines marked by `>` inside a fenced snippet. Treat each note as a
task. The quoted snippet is the reliable part of the anchor, not the line
number: the moment you start editing, the numbers in the header shift, while
the quoted text still says what the user was looking at.

**Draining clears.** A comment is delivered exactly once, so it disappears
from recto the moment you read it. Only run `recto comments` when you are
actually about to act on what comes back. Never run it to check whether
comments exist, never run it and then discard the output, and never run it
twice hoping to re-read something. If you need to know whether notes are
waiting, that is what `ping` is for.

**Do not write comments.** `recto comment` exists, but it is the user's
channel into recto, not yours. Your channel is `annotate`. If you push your
own notes into the comment set you will drain them straight back to yourself
and lose the user's in the noise.

Drain in one of two situations: the user says they left notes or asks you to
address their comments, or a `ping` reports `pending_comments` above zero and
the conversation has moved on to acting on the diff. If you notice pending
comments while doing something else, mention them rather than silently
draining, since draining takes them off the user's screen.

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

`recto comments` is the one command that exits 0 with nothing on stdout: that
means no comments were pending. It exits 1 only when recto is parked in an
editor, where a drain would destroy the notes rather than deliver them. Ask
the user to come back to recto, then try again.

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
      "annotations": 0,
      "pending_comments": 2
    }

The `files` array is always the changed-path list in the diff recto currently
shows. It bounds commands only when their capability scope is `current_diff`.
When focus scope is `workspace`, a path absent from `files` is still a valid
focus target. `scope` is `"range"` for the whole base diff or `"rev"` when
narrowed to a single revision.

`pending_comments` is how you find out the user left you notes. They have no
way to push into your session, so a ping you were already going to run is the
cheapest place to notice. Anything above zero means `recto comments` has
something for you. An older recto that predates the feature omits the field
entirely, which reads as zero.
