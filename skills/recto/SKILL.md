---
name: recto
description: >-
  Drive recto, in a rig's web window or a terminal viewer: highlight code,
  write a literate tour of prose and quoted diff, keep the briefing that
  gathers what needs the person, answer comments sent from the window, lay
  down numbered tour stops, collect private agent notes, and co-author
  durable local review drafts. Load whenever you are explaining or reviewing
  changes, when asked to point at code, when comments arrive from the web
  window, when the user says they left notes, or while collaboratively
  writing PR review comments.
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

Paul reads an authoring rig in a web window beside Rex. `rig serve` shows
the rig his Rex app is on, rendered by `recto serve` and refreshed live:
the briefing, the tours, and each repo's diff. Write to it with plain
`recto` commands, run from inside the repo they're about. With no terminal
Recto running they go straight to Recto's state, which is what the window
reads:

    recto tour < tour.md                       # this repo's story
    recto tour --group "$RIG_BASEDIR" < t.md   # one story across the repos
    recto focus src/app.rs:42-58               # point at code
    recto brief < brief.md                     # the briefing (below)
    recto doc plan < plan.md                   # any other doc, as a tab
    recto clear

The window shows the group's docs as tabs beside the repos: the group tour
first (it's the doc named `tour`), then each `recto doc NAME` in the order
first written, a plan or a PR body, say. Quotes in a doc jump into the repo
tabs, as in a tour. Rewrite a doc to update it; an empty body removes it.
Keep docs few and named for what they are; the briefing, not a doc, is where
anything that needs Paul goes.

The terminal Recto is the escape hatch, for when Paul asks for it or for
what the window can't do yet: retargeting the base and co-authoring review
drafts. `rig recto <repo>` puts that repo's viewer beside the agent,
starting it if needed; any remaining arguments are forwarded to recto from
the correct workspace (`rig recto cloud base 'trunk()'`). Every `rig recto`
command opens the viewer, so don't use it to probe. Do not run tmux pane
commands yourself, and do not use `recto -R` as a substitute: Rig owns the
panes. While a terminal Recto is up, plain `recto` commands drive it
instead, and the window still follows.

Review rigs open with a terminal Recto, since the review slate is the job
there. Route their commands through `rig recto` as before.

### The briefing

The briefing is one page that organizes Paul's attention, pinned above
everything else in the window. Agents can juggle five threads; a person
reading them gets scattered. So instead of answering in many places, gather
it here and keep it current:

    recto brief <<'EOF'
    ## Needs you

    ```ask fold
    Should the revision list start folded, or open once work has landed?
    - folded
    - open when landed
    ```

    ## Done since you last looked

    Comment on `serve.rs:60` (the name of `run`): renamed it to `serve`.

    ## Where we stand

    Comments work end to end on the dev build; shipping is next.
    EOF

Each fenced `ask ID` block is a question Paul answers in place. Its `- `
lines are offered as choices, and he can add words either way. His answer
comes back like any other comment, anchored to the ask's ID. Keep the
section order: what needs him first, then what his comments turned into,
then a short "where we stand". Rewrite the whole briefing rather than
append to it, whenever you finish something he asked for or reach a
decision that's his, and drop asks once they're answered. In a rig it
belongs to `$RIG_BASEDIR`; outside one, pass `--group DIR`. Quotes work as
in a group tour. An empty body removes it.

The chief of staff's window gathers every rig's asks at the top of its
briefing, under a link to the rig that asked, and Paul can answer them
there; the answer reaches you the same way. So an ask left in a briefing
after it's settled (answered in the terminal, say) keeps asking him from
cos's page too: rewrite the briefing to drop it. Link another rig with
`[label](rig:SLUG)`, its basedir name; clicking it puts that rig on screen
in Rex, and the window follows.

### Comments from the window

Paul can select anything in the window (diff lines, a tour passage, part of
the briefing) and comment on it. Comments are held until he sends them, and
then they arrive as one message from "Paul (web window)" with an id, the
anchor, and the quoted selection for each. Mark each one so he can see where
it stands:

    rig ack c3f1a start     # taken up
    rig ack c3f1a done      # handled

Acks carry no text. Don't answer each comment with a message. Put what he
should read (what each comment turned into, questions back) in the briefing.
`rig reply` doesn't answer comments.

Outside a Rig, use the ordinary `recto …` commands below.

## Commands

    recto focus PATH:START-END   # highlight lines START..END
    recto focus PATH:LINE        # highlight a single line
    recto focus PATH             # scroll to the file, no line span
    recto base REVSET            # retarget the diff and wait for it to load
    recto annotate SPEC [SPEC…]  # label multiple spans as numbered steps
    recto tour < FILE            # lay down a literate tour (Markdown on stdin)
    recto tour BODY              # the same, as an argument; empty removes it
    recto tour --group DIR < FILE  # one tour across the repos under DIR
    recto tour-focus N           # show the tour, scrolled to section N
    recto tour-focus             # show the tour where it was left
    recto clear                  # remove the highlight and any annotations
    recto comment-visibility hide # hide non-tour comments from the diff and file tree
    recto comment-visibility show # restore non-tour comments
    recto ping                   # is a recto listening here?
    recto pr OWNER/REPO#NUMBER   # fetch and open public PR context in the TUI
    recto notes                  # collect the private notes the user left for you
    recto review                 # peek at the shared, local-only review draft
    recto review --markdown      # the same slate, as the user reads it
    recto review-focus [summary|ID] # show the Review tab at one item
    recto review-body [--if-revision N] BODY # create, revise, or delete the summary
    recto review-comment PATH:LINE=BODY
    recto review-comment --id ID [--if-revision N] BODY
    recto review-verdict approve|request-changes|comment|unset
    recto state forget --workspace-root PATH

`state forget` is a destructive lifecycle API, not part of ordinary companion
work. Rig calls it while tearing down a workspace. Do not call it merely to
clear a draft or resolve a review, and do not call it without an explicit
workspace-lifecycle request from the user.

PATH is relative to anywhere in the workspace (recto normalizes it to
the repo root). Line numbers are **new-side**: the line numbers in the
post-change file, the ones you'd see in your editor after the edit.

`base` accepts the same backend-native value as startup's `--base`: a jj
revset such as `@-` or `fork_point(trunk() | @)`, or a git ref such as `HEAD`.
Against the live Recto surface the command returns only after the new range is
on screen, so a following `focus` or `annotate` cannot race the old diff. While
Recto is parked in an editor, the command queues the retarget and says that it
will apply when the user returns.

recto shows one surface at a time, and a tab strip across the top says which
ones exist right now. The diff is always there, a tour appears once one is laid
down, and attaching a PR adds two: the Review tab holding the draft review,
then the PR itself. The user switches with `shift-1` through `shift-4`, or by
clicking a tab, and `u` steps back up a level from wherever they are. `ping` reports the current surface as `page`. Both
`focus` and `annotate` switch to the diff, since neither can mean "look here
now" while a different page is up.

Annotate SPECs are `PATH:LINE=label` or `PATH:START-END=label`. Argument
order sets the step numbers, and each call replaces the whole set.

`recto pr` fetches the PR through `gh`, attaches a read-only snapshot, and
switches the diff to where the branch forked off the PR's base branch. That is
the same three-dot comparison GitHub's Files-changed tab renders, so the file
sets agree, and it stays right whether the PR sits on trunk or on another
unmerged branch. Like `base`, the command returns only once the new range is on
screen, so a following `ping`, `focus` or `annotate` cannot race the old diff.
If the base branch has never been fetched into this workspace, Recto attaches
the snapshot but refuses to move the diff and says which commit is missing:
fetch it and reattach, or pick a base yourself with `recto base`. A full
`https://github.com/OWNER/REPO/pull/NUMBER` URL works too. In a Rig review
workspace, Recto asks `rig info --format=json` for the current repository's PR
and performs the same attachment automatically on startup. Other Recto
startups stay offline. The PR overview opens immediately with its description,
timeline, and open review threads in full. Resolved and outdated threads fold
into one-line headings at the end, and stay out of the diff and file pane
entirely. The user toggles between the PR and the diff with `p`, moves among
every public thread with `t` / `T` there (open ones only in the diff), and
presses `enter` on an anchored thread in the diff to open its full
conversation.

Tour stops, tour pull quotes, published threads, shared review drafts, and
private agent notes all appear as typed child rows beneath their changed file
in the file pane.
Moving onto a child with `j` / `k` reveals its anchor in the diff. `enter`
opens a published conversation or the matching draft/note editor; on a tour
stop it returns focus to the revealed span, and on a `❝` pull quote it jumps
into the tour at the section that quotes it. A double click opens the same
objects directly from either their file-pane row or their inline diff content.

## Write a literate tour

`annotate` labels spans where they sit, which caps a tour at one line of prose
per site. A literate tour inverts that: the prose is the document, and the diff
gets quoted into it. Reach for it when the explanation carries the weight and
the code is the evidence, and for `annotate` when the code carries the weight
and the labels are signposts. The two coexist and neither clears the other.

Lay one down by piping Markdown:

    recto tour <<'EOF'
    ## Why the base moved

    Recto used to ask jj for `@-` directly, which broke the moment a stack
    boundary moved underneath us.

    ```recto src/backend.rs:120-138
    ```

    So the resolution now happens once, at load.
    EOF

Top-level headings become the sections, listed in an outline rail with the same
circled badges the diff draws over tour steps. The user reaches them with `1`
through `9`, with `]` and `[`, or by clicking the rail, and the status line
names the one they are in.

Fenced blocks tagged `recto` become pull quotes. Give one a `PATH:LINE` or a
`PATH:START-END` and recto lifts that source out of the diff, syntax
highlighted, with its line numbers and no diff tint. The fence body is ignored,
so leave it empty. `recto` has to stand as its own word in the info string, so
an unrelated `rectoclip` fence stays an ordinary code block. A span that no
longer resolves says so in place rather than vanishing, which matters because a
tour outlives the diff it was written against.

Every quote is a door. `enter` opens the next one below the reader, and a click
opens the one under the pointer — anywhere on its label row, or on a code row's
line-number gutter, so that resting the pointer on the code and clicking does
not navigate. Both land in the full diff with that span focused, and `u` steps
back up into the tour where they left off. Quotes also
appear as `❝` rows under their file in the navigator, so a reviewer already
reading the code can reach the prose about it.

Passing an empty body removes the tour. `clear` deliberately does not, and
neither does a restart: a tour is authored work, and only an explicit request
should discard it.

### A story that spans repos

A tour normally belongs to one repo and quotes only its diff. When the change
is one story told across several repos, write one group tour instead of a tour
per repo:

    recto tour --group "$RIG_BASEDIR" <<'EOF'
    ## The server learns a new route

    ```recto rig/serveweb.go:227-241
    ```

    ## And the renderer answers it

    ```recto recto/src/serve.rs:1-17
    ```
    EOF

DIR is the directory holding the repos (a rig's basedir), and each quote's
path starts with its repo's directory under DIR. The web view (`recto serve`,
behind `rig serve`) shows a group tour above the repo tabs, and a quote opens
that repo's tab at the span. It doesn't go through a viewer, so call `recto`
directly here rather than `rig recto`, and no TUI needs to be running. The TUI
shows only per-repo tours.

Under `rig serve`, the web view keeps a rig's landed work on the page. Once
everything since the commit the rig started from is your own work on trunk
(pushed straight to `main`, say), it diffs from that start instead of the
empty branch point. Tour quotes resolve against that whole range, so a tour
written before the push still reads after it. The page's revision list shows
each commit in the range, marks the landed ones, and narrows to one when
clicked.

The rule for which to write: in a multi-repo rig whose change spans repos,
write one group tour. In a single-repo rig, or when the story really is one
repo's, write that repo's tour. Don't write both for the same story.

### What to actually write

A tour is an argument about a change, not a summary of it. The diff already
lists what moved. The tour says why it moved, and in what order the pieces
start making sense. Write the document you would want handed to you before
reviewing someone else's work.

Aim for four to six sections. The badge keys reach nine, but a tour that needs
nine is usually a document the reader would rather scroll than be walked
through. Give each section one point, and a heading that states that point, so
the outline rail reads as an argument by itself: "Why the base moved" beats
"Changes to backend.rs".

Order by argument, not by file. The file pane already sorts by path, so a tour
that walks files in tree order spends the reader's attention on something they
already had. Start where the change starts making sense, which is often not the
largest file.

Quote the smallest span that carries the point. A pull quote is evidence for a
claim, so make the claim first and let the quote land under it. Twenty lines of
quoted code with no argument around them is just the diff, and the reader can
have the diff in one keypress. A section with no quote at all is fine when its
point is about shape rather than about a particular line.

Say plainly what does not matter. "The remaining thirty files are generated
schema" saves the reader more than another section would, and it tells them you
looked rather than skipped.

Claim only what you checked. Every quote is a real span in a real diff the
reader can open in one keypress, so a wrong claim is not just wrong, it is
visibly wrong, and it costs you the rest of the document. When something comes
from a PR description or a commit message rather than from code you read,
attribute it there instead of asserting it yourself.

## Co-author a public review draft

The review being written is a local draft, not published GitHub content and
not private agent notes: a verdict, a top-level summary, and inline comments.
Recto saves it beneath `$XDG_STATE_HOME/recto/workspaces/`, keyed by the
canonical workspace root, whether or not Rig launched it, and restores only the
draft matching the attached repository, PR number, and head OID. Rig teardown
uses `recto state forget` to end that state with the workspace without learning
Recto's file layout.

The user reads and edits the whole draft on the Review tab, in the order it
will post: the verdict and a count of mechanical blockers (a stale head, a
`TODO`, a comment whose span left the diff), then the summary, then each inline
comment with the diff rows it points at, sorted by file and line. They edit the
item in view with `c` or in `$EDITOR` with `e`, cycle the verdict with `V`, and
press `enter` to see a comment's code in the diff, with `u` to come back. They
can also start an inline comment with `c` on a diff line. Comment badges
(`✎1`, `✎2`, ...) are positions in that order everywhere, so they can shift
as comments arrive; the stable `id` is your handle.

Read the draft with `recto review`; this is a peek, so calling it repeatedly
never consumes anything. Comments come back in posting order, each with its
`id`, `last_editor`, `revision`, and the `snippet` of diff rows the slate
shows. A comment with no `snippet` no longer resolves in the current diff and
cannot post until it is re-anchored or deleted. `recto review --markdown`
prints the same slate as readable Markdown, for reading it back in the
conversation.

Revise the summary with `recto review-body --if-revision N 'new Markdown'`,
and a comment with `recto review-comment --id ID --if-revision N 'new
Markdown'`, passing the revision you last read. A refusal means the user (or
another writer) changed it after your read: it exits 1 and prints the current
draft, so start from their body rather than retrying your text. Passing an
empty body deletes the summary or that comment. Create a comment with
`recto review-comment 'PATH:LINE=Markdown'` or `'PATH:START-END=Markdown'`.
Every mutation returns the updated draft as JSON so both sides see the same
object.

Draft writes never move the user's page, scroll, or cursor. When you want them
looking at something you wrote, say so with `recto review-focus ID`, or
`recto review-focus summary`. `ping` reports `page: "review"` while the slate
is up.

`recto review-verdict` sets the verdict. The verdict is the user's call: set it
when they tell you to, never on your own judgment.

### In the web window

The web window shows the draft too: the verdict and summary at the top of
the repo, and each inline comment under the last line it's about. With no
terminal Recto running, the review commands write the saved draft directly
and answer the same way, and `recto pr URL` attaches the PR for the window.
Paul steers a draft by selecting text in it and commenting. That comment
arrives like any other, anchored as `REPO draft #ID on PATH:LINES` or
`REPO review summary`. Revise with `recto review-comment --id ID` or
`recto review-body`, ack it, and say in the briefing what changed. When the
draft reads as done, ask in the briefing whether to post it; posting is
still a separate step that waits on his answer.

These commands require an attached PR, and this workflow does not post
anything publicly. Do not copy private `recto notes` into the review draft by
assumption: notes are ephemeral direction, while shared drafts are the exact
public-facing prose being co-authored.

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

## Pace a literate tour by following, not leading

`recto tour-focus N` brings the tour up scrolled to section N, numbered from 1
as the rail badges are. `recto tour-focus` on its own just brings it into view.
Asking for a section that does not exist is refused with the count, so the
command is safe to try.

This changes the discipline in the next section rather than obeying it. A span
tour needs you to move a single pointer, so it has to advance one turn at a
time. A literate tour is already written down: the reader can move through it
themselves, at their own speed, and your job is to follow. Lay the whole
document down at once, then use `tour-focus` to put a section on screen as you
talk about it, and `focus` to drop into the diff when a quote deserves the
context around it.

The one-span-per-turn rule below still governs `focus`, because `focus` is
still a pointer.

## Pace span tours one span at a time

Treat every interactive span tour as user-paced. Make showing a span and explaining
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

For a tour with noisy existing review context, use
`recto comment-visibility hide`. This hides published threads, shared drafts,
and private agent notes from the diff and file tree without touching tour
annotations or deleting any content. Prefer explicit `hide` and `show` over
`toggle`, since the user shares this live state with you. Restore comments with
`recto comment-visibility show` when the tour ends unless the user asks to keep
them hidden. Visibility now survives restarts like the rest of the authored
state, so leaving it hidden leaves it hidden; the status line carries a
standing "comments hidden" note so the user can see why. PR and thread pages
remain available when explicitly opened.

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

## Reading the user's agent notes

Everything above points the user's eyes at code. `recto notes` runs the other
way: it hands you private notes they left for the local agent while reading the diff, each
one anchored to a span and quoted with the surrounding lines. They write
those by moving the cursor with `j`/`k` and pressing `n` in recto, which is
worth mentioning if they ask how to give you line-level feedback. Pending
notes and half-written composers survive Recto restarts in every workspace.

    recto notes

The output is markdown on stdout, one numbered section per note, with the
commented lines marked by `>` inside a fenced snippet. Treat each note as a
task. The quoted snippet is the reliable part of the anchor, not the line
number: the moment you start editing, the numbers in the header shift, while
the quoted text still says what the user was looking at. The final line gives
an acknowledgement command containing the stable ids of exactly that set.

**Read, act, acknowledge.** Reading never removes a note, so it is safe to
retry after an interrupted turn. Once every note in that response has been
handled, run the exact command printed at the bottom, for example
`recto notes --ack 4 5`. Acknowledgement removes only those stable ids, so a
new note arriving while you work remains pending. Do not acknowledge first.
If you only need to know whether notes are waiting, use `ping`.

**Do not write agent notes.** `recto note` exists, but it is the user's
private channel into recto, not yours. Your channel is `annotate`. If you push your
own notes into the note set you will read them straight back to yourself
and lose the user's in the noise.

Read notes in one of two situations: the user says they left notes or asks you to
address their comments, or a `ping` reports `pending_comments` above zero and
the conversation has moved on to acting on the diff. If you notice pending
comments while doing something else, mention them rather than silently
reading them.

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
  resolved. For `tour-focus` it means the tour has no such section, and the
  error names how many it has. A failed `base` command also exits 1 with the
  backend's load error. If you know the intended base, retarget with `base` and
  retry once. Do not guess a succession of broader bases just to make a target
  appear.
- **exit 2** — no recto is listening for this workspace (or you're not
  in a repo). Inside a Rig, `rig recto <repo>` starts one, so this
  shouldn't happen there. Outside one, don't keep trying; just describe the
  change in text.

`recto tour`, `recto focus` and `recto clear` are the exceptions: they don't
need a TUI, because the web view (`recto serve`) is a viewer too. With no TUI
running, `focus` saves the span and the web view scrolls to and highlights it;
it still exits 1 for a file that isn't in the diff, and `clear` drops it.

`recto tour` in particular doesn't need a viewer. With none running it
saves the tour where the next viewer and the web view (`recto serve`) read
it, says so on stderr, and exits 0. The stale-review rule still applies, so a
tour against an attached PR whose head doesn't match the workspace exits 1.

`recto notes` is the one command that exits 0 with nothing on stdout: that
means no notes were pending. It exits 1 only when recto is parked in an
editor, where the TUI loop cannot answer the read. Ask
the user to come back to recto, then try again.

The review draft commands (`review`, `review-body`, `review-comment`,
`review-verdict`) also exit 1 while recto is parked in an editor. A write is
refused rather than queued, since you could not have read what it would
replace. A `review-focus` sent meanwhile waits and applies when the user
returns.

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
      "page": "tour",
      "focus": false,
      "annotations": 0,
      "tour": true,
      "tour_sections": 5,
      "comments_visible": true,
      "pending_comments": 2,
      "draft_comments": 1,
      "draft_body": true
    }

The `files` array is always the changed-path list in the diff recto currently
shows. It bounds commands only when their capability scope is `current_diff`.
When focus scope is `workspace`, a path absent from `files` is still a valid
focus target. `scope` is `"range"` for the whole base diff or `"rev"` when
narrowed to a single revision.

During a background range load, `base` and `files` continue to describe the
settled diff and `loading_base` names the requested target. If that load fails,
`loading_base` disappears and `load_error` carries the backend error. Both
fields are absent in the ordinary settled state and in older versions.

`pending_comments` is the backward-compatible wire field that tells you the user left agent notes. They have no
way to push into your session, so a ping you were already going to run is the
cheapest place to notice. Anything above zero means `recto notes` has
something for you. An older recto that predates the feature omits the field
entirely, which reads as zero.

`comments_visible` reports the shared TUI state controlled by `v` and
`recto comment-visibility`. Older Recto versions omit it and always show
comments.

`page` is the surface on screen: `"diff"`, `"tour"`, `"review"`, `"pr"` or
`"thread"`.
`tour` says whether a literate tour is laid down and `tour_sections` counts its
sections, which is the largest number `tour-focus` will take. Both are counted
from the document itself, so they answer before the tour page has ever been
drawn.

`draft_comments` counts the public review comments being co-authored locally.
They survive Recto restarts. It is safe to follow a nonzero count with
`recto review`: that command only peeks and can never remove draft content.

`draft_body` reports whether the same shared review has a top-level body. Like
`draft_comments`, `true` is a reason to peek with `recto review`, never to
publish or delete anything by assumption.
