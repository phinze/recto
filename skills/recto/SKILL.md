---
name: recto
description: Drive a running recto diff viewer from a companion session — scroll to and highlight a file or line span, or lay down a numbered multi-site tour. Load whenever you are explaining, reviewing, or walking through code changes in a workspace where recto might be open, or when asked to "show me where", "point at", or "tour" a diff. Lets you direct the user's eyes to the exact lines you're describing instead of just naming them.
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

## How to use it in a tour

For a single-threaded walkthrough, describe the change in prose and
`focus` each span as you reach it. The highlight is sticky: it stays put
until you focus something else or `recto clear`, so each `focus` call
replaces the last. Re-focusing the same span re-fires its attention
flash, so it's never a no-op. End a tour with `recto clear` so you don't
leave a stray bar on screen.

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

recto is passive: it will not switch the diff base to find your target.
It reports rather than chases.

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
      "focus": false,
      "annotations": 0
    }

The `files` array is the changed-path list in the diff recto currently
shows, so you can check up front whether a path you're about to `focus`
or `annotate` is in scope, instead of firing a throwaway call and
reading the exit code. If your target isn't in `files`, it's almost
always because recto is on a different `base` than you expect: tell the
user to cycle the base with `b`, don't retry blindly. `scope` is
`"range"` for the whole base diff or `"rev"` when narrowed to a single
revision.
