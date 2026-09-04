//! The viewer's state, and everything it can be asked to do.
//!
//! `App` is one struct because the surfaces genuinely share a cursor, a
//! scroll and a diff; splitting it would mean threading the same four things
//! through every call. Its fields are `pub(crate)` rather than private: the
//! panes read them to draw and the input layer reads them to decide what a
//! click meant, and that is the shape of the crate rather than an oversight.

mod composer;

pub(crate) use composer::{ComposerEdit, ComposerKind, Mode, NoteDraft, NoteLayout};

mod files;

pub(crate) use files::{
    FileReviewObject, FileRow, ReviewClick, ReviewClickSurface, build_file_rows,
    file_row_selectable, first_file_row,
};

mod search;

mod review;

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::ListState;

use crate::backend::{Backend, Base, FileChange, FileStatus, Rev, Scope};
use crate::diff::{FetchContent, LineInfo, render_diff};
use crate::highlight::Highlighter;
use crate::ui::chrome::tab_entries;
use crate::ui::diff::{review_thread_span, rows_for_span, step_pointable};
use crate::ui::document::{QuoteSpan, TourQuote, section_step, tour_quote_anchors};
use crate::{link, markdown, parse_pathspec, state, wrap};

pub(crate) struct LoadedDiff {
    pub(crate) workspace_revision: String,
    pub(crate) changes: Vec<FileChange>,
    pub(crate) rendered: Vec<Line<'static>>,
    pub(crate) file_starts: Vec<usize>,
    pub(crate) line_info: Vec<LineInfo>,
    /// Added/removed line counts per file, parallel to `changes`.
    pub(crate) file_stats: Vec<(u32, u32)>,
    /// Populated only when the load was for `Scope::Range`. Rev loads don't
    /// refresh the rev list — selecting a rev shouldn't redraw the strip.
    pub(crate) revs: Option<Vec<Rev>>,
}

pub(crate) const SCROLLOFF: u16 = 3;
/// Rows a wheel tick moves a document page. The diff pane has always moved
/// one row per tick, so anything larger makes the same wheel feel different
/// depending on which page happens to be showing.
pub(crate) const WHEEL_STEP: u16 = 1;
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);
pub(crate) const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);
pub(crate) const STATE_DEBOUNCE: Duration = Duration::from_millis(150);
pub(crate) const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);
/// How long after the pane regains focus a click still counts as the one that
/// focused it. Focus reports and mouse events race, so the window has to cover
/// a click arriving on either side of the focus change.
const FOCUS_CLICK_GRACE: Duration = Duration::from_millis(400);
pub(crate) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub(crate) const SPINNER_FRAME_MS: u128 = 80;
/// How long the arrival flash takes to fade after a focus span lands.
pub(crate) const FOCUS_FLASH: Duration = Duration::from_millis(450);
/// Peak strength of the arrival flash: how far row backgrounds are washed
/// toward mauve at t=0.
pub(crate) const FOCUS_FLASH_ALPHA: f32 = 0.35;
/// Period of the gutter bar's breathing pulse while a focus span is active.
pub(crate) const FOCUS_PULSE_PERIOD: Duration = Duration::from_millis(2200);
/// How far the pulse dims the bar toward the background at its low point.
pub(crate) const FOCUS_PULSE_DEPTH: f32 = 0.55;
/// What the worker is asked to render. The generation distinguishes repeated
/// loads of the same scope, so an older response can never masquerade as the
/// newest request after the view cycles away and back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffRequest {
    pub(crate) generation: u64,
    pub(crate) scope: Scope,
    pub(crate) ignore_ws: bool,
}

pub(crate) struct Loading {
    pub(crate) request: DiffRequest,
    pub(crate) label: String,
    pub(crate) started: Instant,
}

pub(crate) struct Worker {
    pub(crate) request_tx: mpsc::Sender<DiffRequest>,
    pub(crate) response_rx: mpsc::Receiver<(DiffRequest, Result<LoadedDiff>)>,
}

fn spawn_worker(backend: Arc<dyn Backend>, hl: Highlighter) -> Worker {
    spawn_worker_with(move |req| load_diff(&*backend, &hl, req))
}

fn spawn_worker_with(load: impl Fn(&DiffRequest) -> Result<LoadedDiff> + Send + 'static) -> Worker {
    let (request_tx, request_rx) = mpsc::channel::<DiffRequest>();
    let (response_tx, response_rx) = mpsc::channel::<(DiffRequest, Result<LoadedDiff>)>();
    std::thread::spawn(move || {
        while let Ok(mut req) = request_rx.recv() {
            // Only the newest queued view can still reach the screen. An
            // in-flight load cannot be cancelled, but work that piled up
            // behind it can be collapsed before another expensive render.
            for newer in request_rx.try_iter() {
                req = newer;
            }
            let result = load(&req);
            if response_tx.send((req, result)).is_err() {
                break;
            }
        }
    });
    Worker {
        request_tx,
        response_rx,
    }
}

fn load_diff(backend: &dyn Backend, hl: &Highlighter, req: &DiffRequest) -> Result<LoadedDiff> {
    let scope = &req.scope;
    let workspace_revision = backend.workspace_revision()?;
    let changes = backend.list_changes(scope, req.ignore_ws)?;
    let diff = backend.unified_diff(scope, req.ignore_ws)?;
    let revs = match scope {
        Scope::Range(base) => Some(backend.list_revs(base)?),
        Scope::Rev(_) => None,
    };
    // Post-image source per scope: Range's post-image is `@` (jj) or
    // working tree (git), which disk approximates well and cheaply. Rev's
    // post-image is that rev's tree, so we have to ask the backend.
    let fetch: Box<FetchContent> = match scope {
        Scope::Range(_) => Box::new(|path: &str| std::fs::read_to_string(path).ok()),
        Scope::Rev(id) => {
            let id = id.clone();
            Box::new(move |path: &str| backend.file_content(&id, path).ok())
        }
    };
    let rd = render_diff(&diff, &changes, hl, &*fetch);
    Ok(LoadedDiff {
        workspace_revision,
        changes,
        rendered: rd.lines,
        file_starts: rd.file_starts,
        line_info: rd.line_info,
        file_stats: rd.file_stats,
        revs,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Files,
    Diff,
    Commits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Diff,
    Tour,
    PullRequest,
    ReviewThread,
}

impl Focus {
    pub(crate) fn cycle(self, show_files: bool, show_commits: bool) -> Self {
        match self {
            Focus::Files => Focus::Diff,
            Focus::Diff => {
                if show_commits {
                    Focus::Commits
                } else if show_files {
                    Focus::Files
                } else {
                    Focus::Diff
                }
            }
            Focus::Commits => {
                if show_files {
                    Focus::Files
                } else {
                    Focus::Diff
                }
            }
        }
    }
}

/// Visibility policy for a side pane (files / commits). `Auto` derives
/// visibility from the change set — the pane pops in only when there's more
/// than one file / commit to show. `Shown`/`Hidden` are explicit user
/// overrides set by the toggle keys; once set, they survive reloads so the
/// heuristic never re-opens a pane the user just dismissed (or vice versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaneVis {
    Auto,
    Shown,
    Hidden,
}

/// Where the rev cursor is sitting. `All` means "show the full range diff
/// for the current base"; `Rev(i)` narrows to a single rev in `revs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cursor {
    All,
    Rev(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SearchMatch {
    pub(crate) line_idx: usize,
    pub(crate) start: usize, // character offset start
    pub(crate) end: usize,   // character offset end
}

/// A span a companion session asked us to highlight. Stored logically (path +
/// new-side line range) rather than as rendered-row indices, so it survives
/// diff reloads — `focus_rows` re-resolves it against the current render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusSpan {
    pub(crate) path: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
    /// When the span landed; drives the arrival flash and the pulse phase.
    /// Re-focusing the same span resets it — "look here" deserves a fresh
    /// flash even if the eyes-target hasn't moved.
    pub(crate) set_at: Instant,
}

/// A companion-supplied labeled span — one step of a tour. Stored logically
/// (path + new-side line range) like [`FocusSpan`]; `reweave` renders the set
/// as numbered note rows woven into the diff, re-resolving after each reload.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct Annotation {
    pub(crate) path: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) label: String,
}

/// The durable half of a [`FocusSpan`]: the span, without the arrival instant
/// that drives the flash. Restoring one re-fires that flash, which reads as
/// "this is where we were" rather than as a highlight that has gone stale.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct FocusAnchor {
    pub(crate) path: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
}

/// A reviewer-authored note waiting to be handed to an agent. Anchored the same
/// way an [`Annotation`] is, but it flows the other direction: the agent writes
/// annotations for us to read, we write these for the agent to acknowledge.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct AgentNote {
    pub(crate) id: u64,
    pub(crate) path: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) body: String,
}

/// Cached mapping between rendered source lines and the visual rows they
/// occupy after wrapping. `starts[i]` is the first visual row of source line
/// `i`; the final sentinel is the total visual-row count.
#[derive(Default)]
pub(crate) struct DisplayRowIndex {
    pub(crate) width: u16,
    pub(crate) starts: Vec<usize>,
}

impl DisplayRowIndex {
    fn build(rendered: &[Line<'static>], line_info: &[LineInfo], width: u16) -> Self {
        let mut starts = Vec::with_capacity(rendered.len() + 1);
        starts.push(0usize);
        for (idx, line) in rendered.iter().enumerate() {
            let prefix_width = if line_info.get(idx).copied().flatten().is_some() {
                wrap::gutter_prefix_width(line)
            } else {
                wrap::note_prefix_width(line)
            };
            let rows = if width == 0 {
                1
            } else {
                wrap::row_count(line, width, prefix_width)
            };
            starts.push(starts.last().copied().unwrap_or(0).saturating_add(rows));
        }
        Self { width, starts }
    }

    fn total_rows(&self) -> usize {
        self.starts.last().copied().unwrap_or(0)
    }

    fn row_of_line(&self, line: usize) -> usize {
        self.starts
            .get(line)
            .copied()
            .unwrap_or_else(|| self.total_rows())
    }

    fn line_at_row(&self, row: usize) -> Option<(usize, usize)> {
        let total = self.total_rows();
        if total == 0 {
            return None;
        }
        let row = row.min(total - 1);
        let line = self
            .starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1)
            .min(self.starts.len().saturating_sub(2));
        Some((line, row - self.starts[line]))
    }
}

pub(crate) struct App {
    pub(crate) worker: Worker,
    /// Shared with the worker; the app side only uses it for labels.
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) bases: Vec<Base>,
    pub(crate) base_idx: usize,
    /// Index into `revs` of the row being considered as a new base, while the
    /// `b` picker is up. `None` when not picking. Deliberately separate from
    /// `cursor`; see `begin_base_pick`.
    pub(crate) base_pick: Option<usize>,
    /// How many entries of `bases` came from the backend defaults plus
    /// `--base`. Everything past this is the single ad-hoc pick.
    pub(crate) fixed_bases: usize,
    pub(crate) revs: Vec<Rev>,
    pub(crate) cursor: Cursor,
    pub(crate) mode: Mode,
    pub(crate) page: Page,
    pub(crate) pull_request: Option<link::PullRequest>,
    /// Published commit beneath the mutable working copy, refreshed alongside
    /// every diff load so an attached PR can prove it still names this view.
    pub(crate) workspace_revision: String,
    pub(crate) pr_scroll: usize,
    pub(crate) pr_max_scroll: usize,
    /// Outline entries for the PR document — title and the visual row each
    /// section starts at. Rebuilt every draw, since the offsets depend on the
    /// width the body wrapped at.
    pub(crate) pr_sections: Vec<(String, usize)>,
    pub(crate) pr_outline_area: Rect,
    pub(crate) tour_scroll: usize,
    pub(crate) tour_max_scroll: usize,
    pub(crate) tour_sections: Vec<(String, usize)>,
    pub(crate) tour_outline_area: Rect,
    /// Every pull quote as the last draw laid it out, so a click in the tour
    /// body can find the code it points at.
    pub(crate) tour_quotes: Vec<QuoteSpan>,
    pub(crate) tour_body_area: Rect,
    /// Where each of the tour's quotes points. Derived from `tour`, refreshed
    /// with it, and independent of any draw.
    pub(crate) tour_anchors: Vec<TourQuote>,
    /// A section a companion asked for before the tour page had geometry to
    /// resolve it against. Spent by the next draw.
    pub(crate) tour_pending_section: Option<usize>,
    /// Tour scroll to come back to after a quote sent the reader to the diff.
    /// Set on the way in, spent by the first Esc on the way out.
    pub(crate) tour_return: Option<usize>,
    pub(crate) active_thread: Option<usize>,
    pub(crate) thread_scroll: usize,
    pub(crate) thread_max_scroll: usize,
    pub(crate) next_load_generation: u64,
    pub(crate) loading: Option<Loading>,
    pub(crate) reload_pending: bool,
    pub(crate) load_error: Option<String>,
    pub(crate) changes: Vec<FileChange>,
    /// The pristine render as the worker produced it, before annotation note
    /// rows are woven in. `reweave` rebuilds the viewed copies below from
    /// these whenever the diff or the annotation set changes.
    pub(crate) base_rendered: Vec<Line<'static>>,
    pub(crate) base_file_starts: Vec<usize>,
    pub(crate) base_line_info: Vec<LineInfo>,
    pub(crate) rendered: Vec<Line<'static>>,
    pub(crate) file_starts: Vec<usize>,
    pub(crate) line_info: Vec<LineInfo>,
    /// Review object owning each woven render row. Base diff rows and tour
    /// annotations carry `None`; inline thread/draft/note rows retain their
    /// semantic identity so mouse gestures survive wrapping.
    pub(crate) rendered_review_objects: Vec<Option<FileReviewObject>>,
    pub(crate) file_stats: Vec<(u32, u32)>,
    /// Top of the diff viewport in visual-row coordinates. In wrap mode the
    /// display-row index maps this back to a source line and continuation row.
    pub(crate) scroll: usize,
    pub(crate) h_scroll: u16,
    pub(crate) wrap: bool,
    pub(crate) display_rows: DisplayRowIndex,
    pub(crate) diff_viewport: u16,
    pub(crate) focus: Focus,
    pub(crate) file_state: ListState,
    /// File-pane rows in display order (dir headers + files). Rebuilt from
    /// `changes` whenever the change set changes; `file_state` indexes here.
    pub(crate) file_rows: Vec<FileRow>,
    pub(crate) files_area: Rect,
    pub(crate) diff_content_area: Rect,
    pub(crate) commits_area: Rect,
    /// Columns the tab strip occupied in the latest draw, so a click can be
    /// routed to a page. Empty when the strip has nothing to choose between.
    pub(crate) tabs_area: Rect,
    /// The composer body geometry and viewport left behind by the draw pass,
    /// shared by keyboard motion and mouse hit-testing.
    pub(crate) note_layout: NoteLayout,
    pub(crate) commits_state: ListState,
    pub(crate) search_query: Option<String>,
    pub(crate) search_matches: Vec<SearchMatch>,
    pub(crate) search_active_idx: Option<usize>,
    /// Active companion-driven focus, if any. Sticky until replaced or cleared.
    pub(crate) focus_span: Option<FocusSpan>,
    /// Companion-driven tour annotations, in step order. Sticky like
    /// `focus_span`; replaced wholesale by each `annotate` request.
    pub(crate) annotations: Vec<Annotation>,
    /// The literate tour document, as the Markdown the companion sent. Kept
    /// raw because the sections and pull quotes it implies are resolved
    /// against whichever diff is on screen when it renders. Deliberately off
    /// every clear path, like `agent_notes`: it is too expensive to re-author
    /// for Esc to be able to discard it.
    pub(crate) tour: Option<String>,
    /// Private agent notes awaiting acknowledgement, in authoring order. Deliberately not
    /// on any clear path: `clear`, Esc and `q` all drop the agent's tour, and
    /// sweeping up our own undelivered notes alongside it would be data loss.
    /// Explicit acknowledgement is the only thing that empties this.
    pub(crate) agent_notes: Vec<AgentNote>,
    pub(crate) next_agent_note_id: u64,
    /// Durable public review comments shared with the companion agent. These
    /// are local draft content, distinct from both published PR threads and
    /// private agent notes.
    pub(crate) review_draft_comments: Vec<link::DraftReviewComment>,
    /// Optional top-level body for the same shared review draft. Unlike inline
    /// comments it has no file anchor and is authored from the PR overview.
    pub(crate) review_draft_body: Option<link::DraftReviewBody>,
    pub(crate) next_review_draft_id: u64,
    /// XDG-backed durable state keyed by this workspace's canonical root.
    pub(crate) persistence: Option<state::Store>,
    pub(crate) persistence_due: Option<Instant>,
    /// Source-line index of a click-placed edit cursor in the diff, if any.
    /// Distinct from `focus_span` (agent-driven): this is the local "I clicked
    /// here, `e` goes here" marker. Cleared on reload since the index is
    /// position-based, not path-resolved.
    pub(crate) diff_cursor: Option<usize>,
    /// First half of a possible review-object double click. Stored by semantic
    /// object and pane rather than coordinate so redraws cannot retarget it.
    pub(crate) last_review_click: Option<ReviewClick>,
    /// Resolved visibility for each side pane. Derived from `files_vis` /
    /// `commits_vis` plus the current change counts via `resolve_panes`; the
    /// draw and key-handling paths read these bools directly.
    pub show_files: bool,
    pub show_commits: bool,
    /// Visibility policy behind `show_files` / `show_commits`. `Auto` until the
    /// user hits a toggle key, then pinned to their choice.
    pub(crate) files_vis: PaneVis,
    pub(crate) commits_vis: PaneVis,
    /// GitHub-style "ignore whitespace" toggle. When on, diffs are computed
    /// with `-w` (`--ignore-all-space`), collapsing reindentation noise.
    pub(crate) ignore_ws: bool,
    /// Whether non-tour review objects are woven into the diff and file tree.
    /// Durable like the rest of the authored state; the status line carries a
    /// standing "comments hidden" segment so the setting explains itself
    /// instead of relying on being forgotten.
    pub(crate) show_comments: bool,
    /// Whether the keybinding help overlay is up, plus its vertical scroll
    /// position and the maximum established by the latest draw.
    pub(crate) show_help: bool,
    pub(crate) help_scroll: u16,
    pub(crate) help_max_scroll: u16,
    /// Whether our terminal/tmux pane currently has focus. Driven by
    /// focus-change reports; stays `true` on terminals that don't send them.
    pub(crate) terminal_focused: bool,
    /// When focus last came back, so the click that brought the pane forward
    /// can be told apart from the first click meant for what is on screen.
    pub(crate) focus_regained_at: Option<Instant>,
}

impl App {
    pub(crate) fn load(
        backend: Arc<dyn Backend>,
        hl: Highlighter,
        initial: Option<String>,
        persistence: Option<state::Store>,
    ) -> Result<Self> {
        let mut bases = backend.default_bases();
        let base_idx = if let Some(r) = initial {
            if let Some(i) = bases.iter().position(|b| backend.base_label(b) == r) {
                i
            } else {
                bases.insert(0, Base::Revision(r));
                0
            }
        } else {
            0
        };
        let fixed_bases = bases.len();
        let initial_req = DiffRequest {
            generation: 0,
            scope: Scope::Range(bases[base_idx].clone()),
            ignore_ws: false,
        };
        let loaded = load_diff(&*backend, &hl, &initial_req)?;
        let revs = loaded.revs.clone().unwrap_or_default();
        let worker = spawn_worker(backend.clone(), hl);
        let file_rows = build_file_rows(&loaded.changes);
        let display_rows = DisplayRowIndex::build(&loaded.rendered, &loaded.line_info, 0);
        let rendered_review_objects = vec![None; loaded.rendered.len()];
        let mut file_state = ListState::default();
        file_state.select(first_file_row(&file_rows));
        let (agent_notes, next_agent_note_id, restored_note_composer) = persistence
            .as_ref()
            .map(|store| {
                let (notes, next_id, composer) = store.notes();
                (notes.to_vec(), next_id, composer.cloned())
            })
            .unwrap_or_else(|| (Vec::new(), 1, None));
        let tour = persistence
            .as_ref()
            .and_then(|store| store.tour().map(str::to_string));
        let tour_anchors = tour.as_deref().map(tour_quote_anchors).unwrap_or_default();
        let (annotations, restored_focus, show_comments) = persistence
            .as_ref()
            .map(|store| {
                (
                    store.annotations().to_vec(),
                    store.focus().cloned(),
                    store.comments_visible(),
                )
            })
            .unwrap_or_else(|| (Vec::new(), None, true));
        let mut app = Self {
            worker,
            backend,
            bases,
            base_idx,
            base_pick: None,
            fixed_bases,
            revs,
            cursor: Cursor::All,
            mode: restored_note_composer.map_or(Mode::Normal, Mode::NoteInput),
            page: Page::Diff,
            pull_request: None,
            workspace_revision: loaded.workspace_revision,
            pr_scroll: 0,
            pr_max_scroll: 0,
            pr_sections: Vec::new(),
            pr_outline_area: Rect::default(),
            tour_scroll: 0,
            tour_max_scroll: 0,
            tour_sections: Vec::new(),
            tour_outline_area: Rect::default(),
            tour_quotes: Vec::new(),
            tour_body_area: Rect::default(),
            tour_anchors,
            tour_pending_section: None,
            tour_return: None,
            active_thread: None,
            thread_scroll: 0,
            thread_max_scroll: 0,
            next_load_generation: 1,
            loading: None,
            reload_pending: false,
            load_error: None,
            changes: loaded.changes,
            base_rendered: loaded.rendered.clone(),
            base_file_starts: loaded.file_starts.clone(),
            base_line_info: loaded.line_info.clone(),
            rendered: loaded.rendered,
            file_starts: loaded.file_starts,
            line_info: loaded.line_info,
            rendered_review_objects,
            file_stats: loaded.file_stats,
            scroll: 0,
            h_scroll: 0,
            wrap: true,
            display_rows,
            diff_viewport: 0,
            // Overwritten below once resolve_panes settles which panes are up.
            focus: Focus::Diff,
            file_state,
            file_rows,
            files_area: Rect::default(),
            diff_content_area: Rect::default(),
            commits_area: Rect::default(),
            tabs_area: Rect::default(),
            // Plausible stand-in for the one frame between opening the modal
            // and drawing it; the real width lands before any key arrives.
            note_layout: NoteLayout {
                wrap_width: 76,
                ..NoteLayout::default()
            },
            commits_state: ListState::default(),
            search_query: None,
            search_matches: Vec::new(),
            search_active_idx: None,
            focus_span: restored_focus.map(|anchor| FocusSpan {
                path: anchor.path,
                start: anchor.start,
                end: anchor.end,
                set_at: Instant::now(),
            }),
            annotations,
            tour,
            agent_notes,
            next_agent_note_id,
            review_draft_comments: Vec::new(),
            review_draft_body: None,
            next_review_draft_id: 1,
            persistence,
            persistence_due: None,
            diff_cursor: None,
            last_review_click: None,
            show_files: false,
            show_commits: false,
            files_vis: PaneVis::Auto,
            commits_vis: PaneVis::Auto,
            ignore_ws: false,
            show_comments,
            show_help: false,
            help_scroll: 0,
            help_max_scroll: 0,
            terminal_focused: true,
            focus_regained_at: None,
        };
        app.resolve_panes();
        // Land focus on the diff unless the files pane opened on its own.
        app.focus = if app.show_files {
            Focus::Files
        } else {
            Focus::Diff
        };
        if !app.agent_notes.is_empty()
            || !app.annotations.is_empty()
            || !app.tour_anchors.is_empty()
            || !app.show_comments
        {
            app.reweave();
        }
        Ok(app)
    }

    pub(crate) fn base(&self) -> &Base {
        &self.bases[self.base_idx]
    }

    /// Recompute `show_files` / `show_commits` from the visibility policy and
    /// the current change counts, then rescue focus off any pane that just
    /// vanished. Called on every load and reload so `Auto` panes track the
    /// live change set while explicit overrides stay pinned.
    pub(crate) fn resolve_panes(&mut self) {
        self.show_files = match self.files_vis {
            PaneVis::Auto => self.changes.len() > 1,
            PaneVis::Shown => true,
            PaneVis::Hidden => false,
        };
        // `revs` is a ~20-entry history slice for the picker, not the diff's
        // rev set — only the in-range ones count toward "more than one commit".
        let range_revs = self.revs.iter().filter(|r| r.is_in_range).count();
        self.show_commits = match self.commits_vis {
            PaneVis::Auto => range_revs > 1,
            PaneVis::Shown => true,
            PaneVis::Hidden => false,
        };
        if !self.show_files && self.focus == Focus::Files {
            self.focus = Focus::Diff;
        }
        if !self.show_commits && self.focus == Focus::Commits {
            self.focus = Focus::Diff;
        }
    }

    pub(crate) fn toggle_files(&mut self) {
        self.files_vis = if self.show_files {
            PaneVis::Hidden
        } else {
            PaneVis::Shown
        };
        self.resolve_panes();
    }

    /// The scope implied by the current base + cursor. Source of truth for
    /// what we'd ask the backend to load right now.
    fn scope(&self) -> Scope {
        match self.cursor {
            Cursor::All => Scope::Range(self.base().clone()),
            Cursor::Rev(i) => Scope::Rev(self.revs[i].id.clone()),
        }
    }

    /// How a base reads in the header. A base picked out of the rev panel is
    /// a raw change id, so it gets the short form the panel itself displays
    /// rather than 32 hex characters nobody can read at a glance; everything
    /// else defers to the backend's own name for it.
    pub(crate) fn base_text(&self, base: &Base) -> String {
        if let Base::Revision(r) = base
            && let Some(rev) = self.revs.iter().find(|rev| &rev.id == r)
        {
            return rev.short_id.clone();
        }
        self.backend.base_display(base)
    }

    fn scope_label(&self, scope: &Scope) -> String {
        match scope {
            Scope::Range(base) => format!("base: {}", self.base_text(base)),
            Scope::Rev(id) => {
                let short = self
                    .revs
                    .iter()
                    .find(|r| &r.id == id)
                    .map(|r| r.short_id.clone())
                    .unwrap_or_else(|| id.chars().take(8).collect());
                format!("rev: {short}")
            }
        }
    }

    /// Cycle to the next base. Worker loads in the background; current diff
    /// stays visible until the response arrives. Repeated presses advance from
    /// the in-flight target, so a burst of `b`s lands on the right base.
    /// `b`: bring up the rev panel and start picking a base in it. The panel
    /// already renders which rev is the base and what's in range, so it needs
    /// no new screen space to double as the picker.
    ///
    /// Picking gets its own selection rather than reusing the rev cursor,
    /// because the cursor means "what am I looking at" and moving it reloads
    /// the diff. Choosing a base is a question about a rev you are *not*
    /// looking at yet, so conflating the two would make every keystroke of
    /// browsing cost a diff load and land you somewhere you didn't ask for.
    pub(crate) fn begin_base_pick(&mut self) {
        self.commits_vis = PaneVis::Shown;
        self.resolve_panes();
        if !self.show_commits {
            return;
        }
        self.focus = Focus::Commits;
        // Start on the current base. A picker that opens anywhere else makes
        // you find where you already are before you can move.
        self.base_pick = Some(self.revs.iter().position(|r| r.is_base).unwrap_or(0));
    }

    pub(crate) fn base_pick_step(&mut self, delta: isize) {
        let Some(current) = self.base_pick else {
            return;
        };
        if self.revs.is_empty() {
            return;
        }
        let last = self.revs.len() - 1;
        let next = (current as isize + delta).clamp(0, last as isize) as usize;
        self.base_pick = Some(next);
    }

    /// Commit the pick. The panel closes back into normal browsing either way.
    pub(crate) fn confirm_base_pick(&mut self) {
        let Some(i) = self.base_pick.take() else {
            return;
        };
        let Some(rev) = self.revs.get(i) else { return };
        // Re-basing on the rev you're already based on is a no-op worth
        // short-circuiting: it would otherwise cost a full reload to arrive
        // exactly where you started.
        if rev.is_base {
            return;
        }
        self.select_base(Base::Revision(rev.id.clone()));
    }

    fn select_base(&mut self, base: Base) {
        let idx = match self.bases.iter().position(|b| b == &base) {
            Some(i) => i,
            None => {
                // Ad-hoc picks are transient, so only ever one of them is
                // kept. Appending each pick instead would grow `bases` for
                // the life of the session with entries nothing reads back.
                self.bases.truncate(self.fixed_bases);
                self.bases.push(base.clone());
                self.bases.len() - 1
            }
        };
        self.base_idx = idx;
        // Cursor follows the new range — old rev indices won't map to the
        // freshly-loaded revs, so the only safe landing is the overview.
        self.cursor = Cursor::All;
        self.request_scope(Scope::Range(base));
    }

    /// Advance the rev cursor: `All → rev[0] → … → rev[N-1] → All`. No-op if
    /// the range is empty.
    pub(crate) fn cycle_rev_next(&mut self) {
        if self.revs.is_empty() {
            return;
        }
        self.cursor = match self.cursor {
            Cursor::All => Cursor::Rev(0),
            Cursor::Rev(i) if i + 1 >= self.revs.len() => Cursor::All,
            Cursor::Rev(i) => Cursor::Rev(i + 1),
        };
        self.request_current_scope();
    }

    pub(crate) fn cycle_rev_prev(&mut self) {
        if self.revs.is_empty() {
            return;
        }
        self.cursor = match self.cursor {
            Cursor::All => Cursor::Rev(self.revs.len() - 1),
            Cursor::Rev(0) => Cursor::All,
            Cursor::Rev(i) => Cursor::Rev(i - 1),
        };
        self.request_current_scope();
    }

    pub(crate) fn commits_select_next(&mut self) {
        let current_idx = match self.cursor {
            Cursor::All => 0,
            Cursor::Rev(i) => i + 1,
        };
        let max = self.revs.len();
        let next_idx = (current_idx + 1).min(max);
        let new_cursor = if next_idx == 0 {
            Cursor::All
        } else {
            Cursor::Rev(next_idx - 1)
        };
        if new_cursor != self.cursor {
            self.cursor = new_cursor;
            self.request_current_scope();
        }
    }

    pub(crate) fn commits_select_prev(&mut self) {
        let current_idx = match self.cursor {
            Cursor::All => 0,
            Cursor::Rev(i) => i + 1,
        };
        let prev_idx = current_idx.saturating_sub(1);
        let new_cursor = if prev_idx == 0 {
            Cursor::All
        } else {
            Cursor::Rev(prev_idx - 1)
        };
        if new_cursor != self.cursor {
            self.cursor = new_cursor;
            self.request_current_scope();
        }
    }

    pub(crate) fn toggle_commits(&mut self) {
        self.commits_vis = if self.show_commits {
            PaneVis::Hidden
        } else {
            PaneVis::Shown
        };
        self.resolve_panes();
    }

    pub(crate) fn request_current_scope(&mut self) {
        self.request_scope(self.scope());
    }

    fn request_scope(&mut self, scope: Scope) {
        let label = self.scope_label(&scope);
        let request = DiffRequest {
            generation: self.next_load_generation,
            scope,
            ignore_ws: self.ignore_ws,
        };
        self.next_load_generation = self.next_load_generation.wrapping_add(1);
        self.load_error = None;
        match self.worker.request_tx.send(request.clone()) {
            Ok(()) => {
                self.loading = Some(Loading {
                    request,
                    label,
                    started: Instant::now(),
                });
            }
            Err(_) => {
                self.loading = None;
                self.load_error = Some("diff loader stopped unexpectedly".into());
            }
        }
    }

    /// Request a fresh load of the current scope (file watcher). If work is
    /// already in flight, remember that the filesystem changed and reload once
    /// more after the newest requested view settles.
    pub(crate) fn request_reload(&mut self) -> bool {
        if self.loading.is_some() {
            self.reload_pending = true;
            return false;
        }
        self.request_current_scope();
        true
    }

    /// Drain any worker responses. Apply only the one matching the in-flight
    /// target; stale responses (superseded by a newer request) are discarded.
    pub(crate) fn poll_load(&mut self) -> bool {
        let mut changed = false;
        while let Ok((req, result)) = self.worker.response_rx.try_recv() {
            let Some(loading) = self.loading.as_ref() else {
                continue;
            };
            if req.generation != loading.request.generation {
                continue;
            }
            changed = true;
            match result {
                Ok(loaded) => self.apply_loaded(req.scope, loaded),
                Err(error) => {
                    self.loading = None;
                    self.load_error = Some(error.to_string());
                }
            }
            if self.reload_pending {
                self.reload_pending = false;
                self.request_current_scope();
            }
        }
        changed
    }

    /// Whether time alone can change the next frame. Static screens redraw
    /// only in response to state changes; loaders and focus pulses keep their
    /// existing animation cadence.
    pub(crate) fn is_animating(&self) -> bool {
        self.loading.is_some() || self.focus_span.is_some()
    }

    fn apply_loaded(&mut self, scope: Scope, loaded: LoadedDiff) {
        let prev_path = self
            .selected_change()
            .and_then(|i| self.changes.get(i).map(|c| c.path.clone()));

        self.workspace_revision = loaded.workspace_revision;
        if self.review_is_stale() {
            self.focus_span = None;
            self.annotations.clear();
            self.persist_soon();
        }
        self.changes = loaded.changes;
        self.file_rows = build_file_rows(&self.changes);
        self.base_rendered = loaded.rendered;
        self.base_file_starts = loaded.file_starts;
        self.base_line_info = loaded.line_info;
        self.file_stats = loaded.file_stats;
        self.reweave();
        // The cursor is a raw source-line index into the old render; it can't
        // survive a reshuffle, so drop it rather than point it at a stale line.
        self.diff_cursor = None;
        self.last_review_click = None;
        if let Scope::Range(base) = &scope
            && let Some(i) = self.bases.iter().position(|b| b == base)
        {
            self.base_idx = i;
        }
        if let Some(revs) = loaded.revs {
            self.revs = revs;
            if let Cursor::Rev(i) = self.cursor
                && i >= self.revs.len()
            {
                self.cursor = if self.revs.is_empty() {
                    Cursor::All
                } else {
                    Cursor::Rev(self.revs.len() - 1)
                };
            }
        }

        // Counts may have shifted (base cycle, watch-mode edit); let Auto panes
        // pop in or out to match while explicit overrides hold.
        self.resolve_panes();

        let new_idx = prev_path
            .and_then(|p| self.changes.iter().position(|c| c.path == p))
            .or_else(|| (!self.changes.is_empty()).then_some(0));
        match new_idx {
            Some(i) => self.select_change(i),
            None => self.file_state.select(None),
        }

        if let Some(i) = new_idx
            && let Some(&offset) = self.file_starts.get(i)
        {
            self.scroll = self.display_row_of_line(offset).min(self.max_scroll());
        } else {
            self.scroll = 0;
        }
        self.h_scroll = 0;
        self.loading = None;
        if let Some(query) = self.search_query.clone() {
            self.update_search(query);
        }
    }

    fn rebuild_display_rows(&mut self) {
        self.display_rows = DisplayRowIndex::build(
            &self.rendered,
            &self.line_info,
            self.diff_content_area.width,
        );
    }

    pub(crate) fn ensure_display_rows(&mut self, width: u16) {
        if self.display_rows.width != width
            || self.display_rows.starts.len() != self.rendered.len() + 1
        {
            let top = if self.wrap {
                self.display_rows.line_at_row(self.scroll)
            } else {
                None
            };
            self.display_rows = DisplayRowIndex::build(&self.rendered, &self.line_info, width);
            if let Some((line, offset)) = top {
                let start = self.display_rows.row_of_line(line);
                let end = self.display_rows.row_of_line(line.saturating_add(1));
                self.scroll =
                    start.saturating_add(offset.min(end.saturating_sub(start.saturating_add(1))));
            }
        }
    }

    fn display_row_of_line(&self, line: usize) -> usize {
        if self.wrap {
            self.display_rows.row_of_line(line)
        } else {
            line.min(self.rendered.len())
        }
    }

    pub(crate) fn source_line_at_row(&self, row: usize) -> Option<usize> {
        if self.wrap {
            self.display_rows.line_at_row(row).map(|(line, _)| line)
        } else {
            (row < self.rendered.len()).then_some(row)
        }
    }

    pub(crate) fn display_position(&self, row: usize) -> Option<(usize, usize)> {
        if self.wrap {
            self.display_rows.line_at_row(row)
        } else {
            (row < self.rendered.len()).then_some((row, 0))
        }
    }

    fn total_display_rows(&self) -> usize {
        if self.wrap {
            self.display_rows.total_rows()
        } else {
            self.rendered.len()
        }
    }

    fn max_scroll(&self) -> usize {
        let total = self.total_display_rows();
        let overflow = total.saturating_sub(self.diff_viewport as usize);
        if overflow == 0 {
            0
        } else {
            overflow
                .saturating_add(SCROLLOFF as usize)
                .min(total.saturating_sub(1))
        }
    }

    pub(crate) fn scroll_down(&mut self, n: u16) {
        self.scroll = self
            .scroll
            .saturating_add(n as usize)
            .min(self.max_scroll());
    }

    pub(crate) fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n as usize);
    }

    pub(crate) fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    pub(crate) fn scroll_right(&mut self, n: u16) {
        self.h_scroll = self.h_scroll.saturating_add(n);
    }

    pub(crate) fn scroll_left(&mut self, n: u16) {
        self.h_scroll = self.h_scroll.saturating_sub(n);
    }

    /// Resolve the (path, line) the user wants to edit.
    /// Files focus: the selected file's first body line. Diff focus: the line
    /// at the top of the diff viewport. Skips Deleted (the path is gone) and
    /// Renamed/Copied (the jj summary path is not a clean filename).
    pub(crate) fn edit_target(&self) -> Option<(String, u32)> {
        let start = match self.focus {
            Focus::Files => *self.file_starts.get(self.selected_change()?)?,
            // A click-placed cursor wins over the top-of-viewport fallback.
            Focus::Diff => self
                .diff_cursor
                .or_else(|| self.source_line_at_row(self.scroll))?,
            Focus::Commits => self.source_line_at_row(self.scroll)?,
        };
        let (fidx, line) = self
            .line_info
            .iter()
            .skip(start)
            .find_map(|info| info.as_ref().copied())?;
        let change = self.changes.get(fidx)?;
        if matches!(
            change.status,
            FileStatus::Deleted | FileStatus::Renamed | FileStatus::Copied
        ) {
            return None;
        }
        Some((change.path.clone(), line.max(1)))
    }

    /// Move among threads that still anchor to the new side of this diff.
    pub(crate) fn cycle_diff_thread(&mut self, delta: isize) {
        let threads = self.review_thread_rows();
        if threads.is_empty() {
            return;
        }
        let current = self
            .active_thread
            .and_then(|active| threads.iter().position(|(i, _)| *i == active));
        let next = match (current, delta.is_negative()) {
            (Some(i), false) => (i + 1) % threads.len(),
            (Some(i), true) => i.checked_sub(1).unwrap_or(threads.len() - 1),
            (None, false) => 0,
            (None, true) => threads.len() - 1,
        };
        let (thread_idx, rows) = threads[next].clone();
        self.active_thread = Some(thread_idx);
        self.reveal_span(&rows);
        self.diff_cursor = Some(*rows.start());
        self.take_diff_focus();
    }

    /// Whether this click is the one that brought the pane forward. Clicking an
    /// unfocused pane should focus it and nothing else, or a click aimed at the
    /// window lands on whatever the pointer was over.
    pub(crate) fn consume_focus_click(&mut self) -> bool {
        let activating = !self.terminal_focused
            || self
                .focus_regained_at
                .is_some_and(|at| at.elapsed() < FOCUS_CLICK_GRACE);
        if activating {
            // Spent: the next click is a real one, however fast it follows.
            self.focus_regained_at = None;
            self.terminal_focused = true;
        }
        activating
    }

    /// Switch to the nth tab, 1-based. Out of range is a no-op rather than a
    /// clamp: shift+3 on a two-tab strip asked for a tab that isn't there.
    pub(crate) fn select_tab(&mut self, n: usize) {
        if let Some(page) = tab_entries(self).get(n - 1).map(|entry| entry.page) {
            self.page = page;
        }
    }

    /// Which document page's scroll and sections the section keys act on.
    /// Only the PR page and the tour have sections to move through.
    fn document_sections(&self) -> &[(String, usize)] {
        match self.page {
            Page::Tour => &self.tour_sections,
            _ => &self.pr_sections,
        }
    }

    fn document_scroll(&self) -> usize {
        match self.page {
            Page::Tour => self.tour_scroll,
            _ => self.pr_scroll,
        }
    }

    fn set_document_scroll(&mut self, scroll: usize) {
        match self.page {
            Page::Tour => self.tour_scroll = scroll.min(self.tour_max_scroll),
            _ => self.pr_scroll = scroll.min(self.pr_max_scroll),
        }
    }

    /// Open the next pull quote at or below the reader's position. After a
    /// section jump that is the section's own quote, which is what makes
    /// `enter` mean "show me the code this part is about" without needing a
    /// separate cursor to move around.
    pub(crate) fn open_quote_in_view(&mut self) -> link::Response {
        let spec = self
            .tour_quotes
            .iter()
            .find(|quote| quote.rows.end > self.tour_scroll)
            .map(|quote| quote.spec.clone());
        match spec {
            Some(spec) => self.open_quote(&spec),
            None => link::Response::err("no pull quote below here"),
        }
    }

    /// Follow a pull quote into the full diff, remembering where in the tour
    /// the reader left so Esc can put them back.
    pub(crate) fn open_quote(&mut self, spec: &str) -> link::Response {
        let (path, start, end) = parse_pathspec(spec);
        let Some(start) = start else {
            return link::Response::err(format!("quote names no line: {spec}"));
        };
        let path = path.to_string();
        self.tour_return = Some(self.tour_scroll);
        self.page = Page::Diff;
        self.focus_target(&path, Some(start), end)
    }

    /// Step back up one level of the review surface: a quote to the tour it
    /// came from, the tour or PR to the diff, a thread to its PR.
    ///
    /// Unlike Esc this only ever navigates. It will not clear a search, drop a
    /// highlight, discard annotations or reach the quit confirmation, so it
    /// stays safe to press without checking what else it might mean.
    pub(crate) fn go_up(&mut self) {
        match self.page {
            Page::Diff => {
                self.return_to_tour();
            }
            Page::Tour | Page::PullRequest => self.page = Page::Diff,
            Page::ReviewThread => self.page = Page::PullRequest,
        }
    }

    /// Step back out of a quote, if one brought us here. Reported so Esc can
    /// fall through to its usual unwinding when it did not.
    pub(crate) fn return_to_tour(&mut self) -> bool {
        let Some(scroll) = self.tour_return.take() else {
            return false;
        };
        if self.tour.is_none() {
            return false;
        }
        self.tour_scroll = scroll;
        self.page = Page::Tour;
        true
    }

    /// Move one section forward or back on the current document page.
    pub(crate) fn jump_section(&mut self, delta: isize) {
        let scroll = section_step(self.document_sections(), self.document_scroll(), delta);
        if let Some(scroll) = scroll {
            self.set_document_scroll(scroll);
        }
    }

    /// Jump straight to a section by its badge number, 0-based internally.
    pub(crate) fn jump_to_section(&mut self, index: usize) {
        let scroll = self
            .document_sections()
            .get(index)
            .map(|(_, offset)| *offset);
        if let Some(scroll) = scroll {
            self.set_document_scroll(scroll);
        }
    }

    /// Bring the tour into view, optionally at a numbered section.
    ///
    /// Section offsets depend on the wrap width, which only the draw pass
    /// knows, so the number is validated against the document's heading count
    /// — which needs no geometry — and the scroll itself is left for the next
    /// draw to apply.
    fn focus_tour_section(&mut self, section: Option<usize>) -> link::Response {
        let Some(source) = self.tour.as_deref() else {
            return link::Response::err("no tour is laid down");
        };
        let count = markdown::outlined(source).sections.len();
        if let Some(n) = section {
            if n == 0 || n > count {
                return link::Response::err(format!(
                    "tour has {count} section{}; no section {n}",
                    if count == 1 { "" } else { "s" }
                ));
            }
            self.tour_pending_section = Some(n - 1);
        }
        self.page = Page::Tour;
        link::Response::ok()
    }

    /// Replace or remove the literate tour. An empty body removes it, the same
    /// way an empty review body deletes that draft.
    fn set_tour(&mut self, body: String) -> link::Response {
        let body = body.trim().to_string();
        self.tour = (!body.is_empty()).then_some(body);
        self.tour_anchors = self
            .tour
            .as_deref()
            .map(tour_quote_anchors)
            .unwrap_or_default();
        self.rebuild_file_rows();
        self.persist_soon();
        link::Response::ok()
    }

    /// Move among every public thread in the attached snapshot, including
    /// outdated and left-side conversations that cannot be pinned in this diff.
    pub(crate) fn cycle_public_thread(&mut self, delta: isize) -> bool {
        let Some(len) = self
            .pull_request
            .as_ref()
            .map(|pr| pr.threads.len())
            .filter(|len| *len > 0)
        else {
            return false;
        };
        let next = match (self.active_thread.filter(|i| *i < len), delta.is_negative()) {
            (Some(i), false) => (i + 1) % len,
            (Some(i), true) => i.checked_sub(1).unwrap_or(len - 1),
            (None, false) => 0,
            (None, true) => len - 1,
        };
        self.active_thread = Some(next);
        true
    }

    fn thread_at_cursor(&self) -> Option<usize> {
        let (path, line) = self.cursor_target()?;
        let threads = &self.pull_request.as_ref()?.threads;
        let contains_cursor = |thread: &link::ReviewThread| {
            thread.path == path
                && review_thread_span(thread)
                    .is_some_and(|(start, end)| (start..=end).contains(&line))
        };
        self.active_thread
            .filter(|i| threads.get(*i).is_some_and(contains_cursor))
            .or_else(|| threads.iter().position(contains_cursor))
    }

    pub(crate) fn open_thread_at_cursor(&mut self) -> bool {
        let Some(thread) = self.thread_at_cursor() else {
            return false;
        };
        self.active_thread = Some(thread);
        self.thread_scroll = 0;
        self.page = Page::ReviewThread;
        true
    }

    /// Jump to annotation step `i` (0-based) — the number-key navigation.
    pub(crate) fn jump_to_annotation(&mut self, i: usize) {
        let Some(a) = self.annotations.get(i).cloned() else {
            return;
        };
        let Some(file_idx) = self.changes.iter().position(|c| c.path == a.path) else {
            return;
        };
        if let Some(rows) = rows_for_span(&self.line_info, file_idx, a.start, a.end) {
            self.reveal_span(&rows);
            self.take_diff_focus();
        }
    }

    /// Step the diff cursor `delta` real rows, seeding it at the top of the
    /// viewport if it isn't placed yet. Only rows carrying line info are
    /// candidates: hunk headers, file separators and woven note rows are things
    /// you can look at but not point at, so the cursor walks past them.
    pub(crate) fn cursor_step(&mut self, delta: isize) {
        let pointable = |app: &Self, i: usize| app.line_info.get(i).copied().flatten().is_some();
        let Some(seed) = self
            .diff_cursor
            .or_else(|| self.source_line_at_row(self.scroll))
        else {
            self.scroll_by(delta);
            return;
        };

        // The first press places the cursor rather than moving it, so it
        // appears where the eye already is instead of a row further on.
        if self.diff_cursor.is_none() {
            match (seed..self.line_info.len()).find(|&i| pointable(self, i)) {
                Some(line) => {
                    self.diff_cursor = Some(line);
                    self.reveal_cursor();
                }
                None => self.scroll_by(delta),
            }
            return;
        }

        match step_pointable(&self.line_info, seed, delta) {
            Some(line) => {
                self.diff_cursor = Some(line);
                self.reveal_cursor();
            }
            // Already against the first or last pointable row; let the key fall
            // back to plain scrolling so it still does something.
            None => self.scroll_by(delta),
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        if delta >= 0 {
            self.scroll_down(delta as u16);
        } else {
            self.scroll_up((-delta) as u16);
        }
    }

    /// Keep the cursor inside the viewport, honouring the same scrolloff the
    /// rest of the UI uses. Only moves the scroll when the cursor would
    /// otherwise sit in (or past) the margin, so reading stays still.
    fn reveal_cursor(&mut self) {
        let Some(line) = self.diff_cursor else {
            return;
        };
        let height = self.diff_content_area.height as usize;
        if height == 0 {
            return;
        }
        let row = self.display_row_of_line(line);
        let margin = (SCROLLOFF as usize).min(height.saturating_sub(1) / 2);
        if row < self.scroll + margin {
            self.scroll = row.saturating_sub(margin);
        } else if row + margin >= self.scroll + height {
            self.scroll = (row + margin + 1).saturating_sub(height);
        }
        self.clamp_scroll();
    }

    /// The file and new-side line the cursor sits on — the anchor a comment
    /// gets pinned to. Falls back to the top of the viewport when no cursor has
    /// been placed, matching how `e` picks its target.
    pub(crate) fn cursor_target(&self) -> Option<(String, u32)> {
        let line = self
            .diff_cursor
            .or_else(|| self.source_line_at_row(self.scroll))?;
        let (file_idx, line_no) = self
            .line_info
            .iter()
            .skip(line)
            .find_map(|info| info.as_ref().copied())?;
        Some((self.changes.get(file_idx)?.path.clone(), line_no))
    }

    /// Index into `changes` of the file owning the current scroll position.
    pub(crate) fn current_file(&self) -> Option<usize> {
        let source_line = self.source_line_at_row(self.scroll)?;
        self.file_starts
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &start)| start <= source_line)
            .map(|(i, _)| i)
    }

    pub(crate) fn toggle_wrap(&mut self) {
        let top_line = self.source_line_at_row(self.scroll).unwrap_or(0);
        self.wrap = !self.wrap;
        if self.wrap {
            self.h_scroll = 0;
        }
        self.scroll = self.display_row_of_line(top_line).min(self.max_scroll());
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use anyhow::anyhow;
    use crossterm::event::{self, MouseEventKind};
    use ratatui::Terminal;

    use crate::diff::{augment_hunk_header, hunk_header, parse_hunk_starts};
    use crate::testing::change;
    use crate::ui::chrome::draw;
    use crate::ui::diff::{
        agent_note_index_at, agent_note_line, body_text, gutter_signature, note_line,
    };

    use super::*;
    use ratatui::layout::Position;

    use crate::input::{handle_mouse, move_note_caret_to_click};
    use crate::testing::*;
    use crate::ui::document::active_section;
    use std::sync::atomic::Ordering;

    fn test_request(generation: u64) -> DiffRequest {
        DiffRequest {
            generation,
            scope: Scope::Range(Base::Revision("base".into())),
            ignore_ws: false,
        }
    }

    fn settle_load(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.loading.is_some() {
            app.poll_load();
            assert!(Instant::now() < deadline, "loader did not settle");
            std::thread::yield_now();
        }
    }

    #[test]
    fn worker_skips_superseded_queued_loads() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = spawn_worker_with(move |req| {
            seen.lock().unwrap().push(req.generation);
            started_tx.send(req.generation).unwrap();
            if req.generation == 1 {
                release_rx.recv().unwrap();
            }
            Err(anyhow!("synthetic load result"))
        });

        worker.request_tx.send(test_request(1)).unwrap();
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        worker.request_tx.send(test_request(2)).unwrap();
        worker.request_tx.send(test_request(3)).unwrap();
        release_tx.send(()).unwrap();

        assert_eq!(
            worker
                .response_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .0
                .generation,
            1
        );
        assert_eq!(started_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 3);
        assert_eq!(
            worker
                .response_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .0
                .generation,
            3
        );
        assert_eq!(*calls.lock().unwrap(), vec![1, 3]);
    }

    #[test]
    fn filesystem_change_during_load_gets_a_followup_load() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();

        app.request_current_scope();
        assert!(!app.request_reload());
        assert!(app.reload_pending);
        settle_load(&mut app);

        assert_eq!(backend.loads.load(Ordering::SeqCst), 3);
        assert!(!app.reload_pending);
        assert!(app.load_error.is_none());
    }

    #[test]
    fn reader_wraps_by_default_and_toggle_unwraps() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();

        assert!(app.wrap);
        app.toggle_wrap();
        assert!(!app.wrap);
    }

    /// A lone tab is a label rather than a choice, so the strip stays out of
    /// the way until a second surface exists to switch to.
    #[test]
    fn the_tab_strip_costs_a_row_only_once_there_is_somewhere_to_go() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let alone = app.diff_content_area.height;
        assert_eq!(app.tabs_area, Rect::default());

        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.diff_content_area.height + 1, alone);
        assert_eq!(app.tabs_area.height, 1);
    }

    #[test]
    fn clicking_a_tab_switches_pages() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let entries = tab_entries(&app);
        let column = |page: Page| {
            entries
                .iter()
                .find(|entry| entry.page == page)
                .expect("tab present")
                .columns
                .start
        };
        let (diff_col, pr_col) = (column(Page::Diff), column(Page::PullRequest));
        let tab_row = app.tabs_area.y;

        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::PullRequest);

        handle_mouse(&mut app, left_click(diff_col, tab_row));
        assert_eq!(app.page, Page::Diff);
    }

    /// Restoring reads the snapshot off disk. `App::load` never calls `github`,
    /// so this passing at all is the offline-startup guarantee.
    #[test]
    fn a_saved_pull_request_restores_with_its_drafts() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-prsnap", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");

        let backend = Arc::new(TestBackend::new());
        let store = state::Store::load_at(&state_home, &root, None).unwrap();
        let mut app = App::load(backend.clone(), Highlighter::new(), None, Some(store)).unwrap();
        assert!(
            app.attach_pull_request(empty_pull_request("base"), false)
                .ok
        );
        app.set_review_draft_body(
            "Typed and then the process died".into(),
            link::DraftEditor::User,
        );
        app.persist_now().unwrap();

        let reopened = state::Store::load_at(&state_home, &root, None).unwrap();
        let snapshot = reopened.pull_request().cloned().expect("snapshot saved");
        assert_eq!(snapshot.number, 42);

        let mut restarted = App::load(backend, Highlighter::new(), None, Some(reopened)).unwrap();
        assert!(restarted.attach_pull_request(snapshot, false).ok);
        assert_eq!(
            restarted
                .review_draft_body
                .as_ref()
                .map(|body| body.body.as_str()),
            Some("Typed and then the process died"),
            "drafts are keyed to the PR, so they come back with it"
        );
    }

    /// The restart case Paul actually cares about: an agent lays down a tour,
    /// the viewer dies, and the typed words are still there on the way back.
    #[test]
    fn a_restart_brings_back_the_tour_and_its_annotations() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-durable", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");

        let backend = Arc::new(TestBackend::new());
        let store = state::Store::load_at(&state_home, &root, None).unwrap();
        let mut app = App::load(backend.clone(), Highlighter::new(), None, Some(store)).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## Why the base moved\n\nProse.".into(),
        });
        app.handle_request(link::Request::Annotate {
            sites: vec![link::Site {
                path: "src/main.rs".into(),
                start: 12,
                end: None,
                label: "Step 1: the guard".into(),
            }],
        });
        app.handle_request(link::Request::CommentVisibility {
            visible: Some(false),
        });
        app.persist_now().unwrap();

        let reopened = state::Store::load_at(&state_home, &root, None).unwrap();
        let restarted = App::load(backend, Highlighter::new(), None, Some(reopened)).unwrap();

        assert_eq!(
            restarted.tour.as_deref(),
            Some("## Why the base moved\n\nProse.")
        );
        assert_eq!(restarted.annotations.len(), 1);
        assert_eq!(restarted.annotations[0].label, "Step 1: the guard");
        assert!(!restarted.show_comments, "visibility is durable too");
    }

    #[test]
    fn an_empty_tour_body_takes_the_tour_down() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();

        app.handle_request(link::Request::Tour {
            body: "## Why the base moved\n\nProse.".into(),
        });
        assert_eq!(app.tour.as_deref(), Some("## Why the base moved\n\nProse."));
        assert!(app.status().tour);

        app.handle_request(link::Request::Tour { body: "   ".into() });
        assert_eq!(app.tour, None);
        assert!(!app.status().tour);
    }

    /// `clear` tidies the agent's own pointer and labels. A tour is authored
    /// content like an unread agent note, so it is deliberately off that path:
    /// Esc must not be able to discard a document that cost real work.
    #[test]
    fn clear_leaves_the_tour_standing() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## Step one".into(),
        });
        app.annotations.push(Annotation {
            path: "src/main.rs".into(),
            start: 1,
            end: 1,
            label: "step".into(),
        });

        app.handle_request(link::Request::Clear);
        assert!(app.annotations.is_empty(), "clear still drops annotations");
        assert_eq!(app.tour.as_deref(), Some("## Step one"));
    }

    /// A tour quotes the diff, so a stale review makes its quotes meaningless
    /// and laying one down is refused like focus and annotate. Taking one down
    /// stays allowed, or a stale tour could never be cleaned up.
    #[test]
    fn a_stale_review_refuses_a_new_tour_but_allows_removing_one() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();
        assert!(
            app.attach_pull_request(empty_pull_request("base"), false)
                .ok
        );
        app.handle_request(link::Request::Tour {
            body: "## Before the drift".into(),
        });
        backend.set_revision("def456");

        let refused = app.handle_request(link::Request::Tour {
            body: "## After the drift".into(),
        });
        assert!(!refused.ok);
        assert_eq!(app.tour.as_deref(), Some("## Before the drift"));

        let removed = app.handle_request(link::Request::Tour {
            body: String::new(),
        });
        assert!(removed.ok);
        assert_eq!(app.tour, None);
    }

    #[test]
    fn clicking_a_pull_quote_opens_the_diff_and_esc_comes_back() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## Step\n\nProse.\n\n```recto foo.go:111\n```\n\nAfter.".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.tour_quotes.len(), 1);
        let quote = app.tour_quotes[0].clone();
        assert_eq!(quote.spec, "foo.go:111");
        // Label row plus at least one lifted diff row.
        assert!(quote.rows.len() > 1, "the quote expanded to real diff rows");
        assert!(quote.gutter > 0, "lifted rows carry a line-number gutter");

        // Content starts past the border and the block's padding; the first
        // body row sits one below the border.
        let content_x = app.tour_body_area.x + 2;
        let code_y = app.tour_body_area.y + 1 + quote.code as u16;

        // Resting the mouse on the code and clicking is how a reader loses
        // their place, so the code is not a target.
        handle_mouse(&mut app, left_click(content_x + quote.gutter, code_y));
        assert_eq!(app.page, Page::Tour, "the code itself does not navigate");

        // The gutter beside it does.
        handle_mouse(&mut app, left_click(content_x + quote.gutter - 1, code_y));
        assert_eq!(app.page, Page::Diff, "a quote gutter drills into the diff");
        assert!(app.return_to_tour());

        // And so does the label row, end to end, so the affordance is
        // findable without hunting for the narrow band.
        let label_y = app.tour_body_area.y + 1 + quote.rows.start as u16;
        handle_mouse(&mut app, left_click(content_x + quote.gutter + 4, label_y));

        assert_eq!(
            app.page,
            Page::Diff,
            "a quote label drills into the full diff"
        );
        assert_eq!(
            app.focus_span
                .as_ref()
                .map(|span| (span.path.as_str(), span.start)),
            Some(("foo.go", 111))
        );

        assert!(app.return_to_tour(), "Esc unwinds back into the tour");
        assert_eq!(app.page, Page::Tour);
        assert!(!app.return_to_tour(), "and the return is spent once used");
    }

    /// After a section jump the next quote below is that section's own, which
    /// is what makes a single `enter` binding predictable without a cursor.
    #[test]
    fn enter_opens_the_quote_belonging_to_the_section_in_view() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: concat!(
                "## First\n\nprose\n\n```recto foo.go:2\n```\n\n",
                "## Second\n\nprose\n\n```recto foo.go:111\n```\n"
            )
            .into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.tour_quotes.len(), 2);

        // From the top, the first quote.
        assert!(app.open_quote_in_view().ok);
        assert_eq!(
            app.focus_span.as_ref().map(|span| span.start),
            Some(2),
            "the first section's quote"
        );

        assert!(app.return_to_tour());
        app.jump_to_section(1);
        assert!(app.open_quote_in_view().ok);
        assert_eq!(
            app.focus_span.as_ref().map(|span| span.start),
            Some(111),
            "the second section's quote"
        );

        // Past the last quote there is nothing to open, and nothing moves.
        assert!(app.return_to_tour());
        app.tour_scroll = app.tour_max_scroll;
        let refused = app.open_quote_in_view();
        assert!(!refused.ok || app.page == Page::Diff);
    }

    #[test]
    fn u_steps_up_one_level_and_never_quits() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        app.pull_request = Some(empty_pull_request("base"));
        app.handle_request(link::Request::Tour {
            body: "## Step\n\nprose\n\n```recto foo.go:111\n```\n".into(),
        });

        // A quote drilled into the diff steps back into the tour. The quote's
        // rendered position only exists after a draw, as it does in use.
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.open_quote_in_view().ok);
        assert_eq!(app.page, Page::Diff);
        app.go_up();
        assert_eq!(app.page, Page::Tour);

        // The tour and the PR both step back to the diff.
        app.go_up();
        assert_eq!(app.page, Page::Diff);
        app.page = Page::PullRequest;
        app.go_up();
        assert_eq!(app.page, Page::Diff);

        // A thread steps up to the PR it belongs to.
        app.page = Page::ReviewThread;
        app.go_up();
        assert_eq!(app.page, Page::PullRequest);

        // And on a diff nobody drilled into, it is simply a no-op: no quit
        // confirmation, no cleared search, no discarded highlight.
        app.page = Page::Diff;
        app.search_query = Some("needle".into());
        app.annotations.push(Annotation {
            path: "foo.go".into(),
            start: 111,
            end: 111,
            label: "step".into(),
        });
        app.go_up();
        assert_eq!(app.page, Page::Diff);
        assert!(matches!(app.mode, Mode::Normal), "never reaches quit");
        assert_eq!(app.search_query.as_deref(), Some("needle"));
        assert_eq!(app.annotations.len(), 1);
    }

    /// Clicking an unfocused pane aims at the window, not at whatever the
    /// pointer happens to be over.
    #[test]
    fn the_click_that_focuses_the_pane_does_nothing_else() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let entries = tab_entries(&app);
        let pr_col = entries
            .iter()
            .find(|entry| entry.page == Page::PullRequest)
            .expect("tab present")
            .columns
            .start;
        let tab_row = app.tabs_area.y;

        // Unfocused: the click brings the pane forward and stops there.
        app.terminal_focused = false;
        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::Diff, "the activating click is swallowed");
        assert!(app.terminal_focused, "but it does focus the pane");

        // Spent: the very next click is a real one.
        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::PullRequest);
    }

    /// A focus report can arrive before the click that caused it, so the grace
    /// window has to catch that ordering too.
    #[test]
    fn a_click_just_after_a_focus_report_is_still_the_focusing_click() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let entries = tab_entries(&app);
        let pr_col = entries
            .iter()
            .find(|entry| entry.page == Page::PullRequest)
            .expect("tab present")
            .columns
            .start;
        let tab_row = app.tabs_area.y;

        // Focus already reported, click lands right behind it.
        app.terminal_focused = true;
        app.focus_regained_at = Some(Instant::now());
        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::Diff);

        handle_mouse(&mut app, left_click(pr_col, tab_row));
        assert_eq!(app.page, Page::PullRequest);
    }

    /// Scrolling was never aimed at the window, so it keeps working.
    #[test]
    fn scrolling_an_unfocused_pane_still_scrolls() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        app.terminal_focused = false;
        let wheel = event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: app.tour_body_area.x + 2,
            row: app.tour_body_area.y + 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        handle_mouse(&mut app, wheel);
        assert!(app.tour_scroll > 0);
    }

    /// The way back: a reviewer already looking at a file can reach the prose
    /// about it without hunting for the right section in the tab.
    #[test]
    fn a_quote_row_jumps_into_the_tour_section_that_owns_it() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        app.handle_request(link::Request::Tour {
            body: concat!(
                "## First\n\nprose\n\n```recto foo.go:2\n```\n\n",
                "## Second\n\nprose\n\n```recto foo.go:111\n```\n"
            )
            .into(),
        });

        // Anchors come from the source, so they exist before any draw.
        assert_eq!(app.tour_anchors.len(), 2);
        assert_eq!(app.tour_anchors[1].section, 1);
        assert_eq!(app.tour_anchors[1].start, 111);

        // And they are listed under their file in the navigator.
        let quote_rows: Vec<usize> = app
            .file_rows
            .iter()
            .filter_map(|row| match row {
                FileRow::ReviewObject {
                    object: FileReviewObject::TourQuote(i),
                    ..
                } => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(quote_rows, vec![0, 1]);

        app.activate_review_object(FileReviewObject::TourQuote(1));
        assert_eq!(app.page, Page::Tour);
        assert_eq!(app.tour_pending_section, Some(1));
    }

    #[test]
    fn tour_focus_validates_against_the_documents_headings() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });

        // Counted from the document, so it answers before any draw.
        let status = app.handle_request(link::Request::Ping).status.unwrap();
        assert_eq!(status.tour_sections, 3);
        assert_eq!(status.page, "diff");

        let past_the_end = app.handle_request(link::Request::TourFocus { section: Some(4) });
        assert!(!past_the_end.ok);
        assert_eq!(app.page, Page::Diff, "a refused focus moves nothing");

        let ok = app.handle_request(link::Request::TourFocus { section: Some(3) });
        assert!(ok.ok);
        assert_eq!(app.page, Page::Tour);
        assert_eq!(app.tour_pending_section, Some(2));
    }

    /// The offsets a section jump needs only exist after a draw, so the request
    /// defers the scroll rather than silently landing on nothing.
    #[test]
    fn a_deferred_section_lands_on_the_next_draw() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });

        assert!(
            app.handle_request(link::Request::TourFocus { section: Some(3) })
                .ok
        );
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.tour_pending_section, None, "spent by the draw");
        assert_eq!(app.tour_scroll, app.tour_sections[2].1);
        assert_eq!(active_section(&app.tour_sections, app.tour_scroll), Some(2));
    }

    /// "Look here now" cannot mean anything while another page is up.
    #[test]
    fn focus_brings_the_diff_back_into_view() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        load_sample_diff(&mut app);
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb".into(),
        });
        assert!(
            app.handle_request(link::Request::TourFocus { section: None })
                .ok
        );
        assert_eq!(app.page, Page::Tour);

        let focused = app.handle_request(link::Request::Focus {
            path: "foo.go".into(),
            start: Some(111),
            end: None,
        });
        assert!(focused.ok);
        assert_eq!(app.page, Page::Diff);
    }

    #[test]
    fn sections_stay_reachable_when_the_document_fits() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(159, 77);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb\n\n## Three\n\nc".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let third = app.tour_sections[2].1;
        assert!(third > 0, "sections sit at distinct offsets");

        app.jump_to_section(2);
        assert_eq!(app.tour_scroll, third, "the last section is reachable");
        assert_eq!(active_section(&app.tour_sections, app.tour_scroll), Some(2));

        app.jump_section(-1);
        assert_eq!(app.tour_scroll, app.tour_sections[1].1);
    }

    #[test]
    fn a_tour_earns_a_tab_between_the_diff_and_the_pr() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        let pages =
            |app: &App| -> Vec<Page> { tab_entries(app).into_iter().map(|e| e.page).collect() };
        assert_eq!(pages(&app), vec![Page::Diff, Page::PullRequest]);

        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb".into(),
        });
        assert_eq!(pages(&app), vec![Page::Diff, Page::Tour, Page::PullRequest]);

        app.select_tab(2);
        assert_eq!(app.page, Page::Tour);
    }

    #[test]
    fn the_tour_page_renders_its_sections_with_a_rail() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        let filler = (1..=30)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        app.handle_request(link::Request::Tour {
            body: format!(
                "## Why the base moved\n\n{filler}\n\n## What the viewer sees\n\n{filler}"
            ),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.tour_sections.len(), 2);
        assert_eq!(app.tour_sections[0].0, "Why the base moved");
        assert_eq!(app.tour_sections[1].0, "What the viewer sees");
        assert!(app.tour_outline_area.width > 0, "wide page gets a rail");
        assert!(app.tour_max_scroll > 0, "document overflows");

        let second = app.tour_sections[1].1;
        app.jump_to_section(1);
        assert_eq!(app.tour_scroll, second.min(app.tour_max_scroll));
        // Section keys act on the page you are on, not on the PR's sections.
        assert_eq!(app.pr_scroll, 0);
    }

    /// Taking the tour down while reading it must not strand the viewer on a
    /// page that no longer has anything to show.
    #[test]
    fn removing_the_tour_falls_back_to_the_diff() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.handle_request(link::Request::Tour {
            body: "## One\n\na\n\n## Two\n\nb".into(),
        });
        app.page = Page::Tour;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.page, Page::Tour);

        app.handle_request(link::Request::Tour {
            body: String::new(),
        });
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.page, Page::Diff);
    }

    #[test]
    fn a_tab_number_past_the_strip_is_a_no_op() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pull_request = Some(empty_pull_request("base"));

        app.select_tab(2);
        assert_eq!(app.page, Page::PullRequest);
        app.select_tab(1);
        assert_eq!(app.page, Page::Diff);
        app.select_tab(3);
        assert_eq!(app.page, Page::Diff, "no third tab to land on");
    }

    #[test]
    fn a_section_number_scrolls_straight_to_its_heading() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pr_sections = vec![("A".into(), 0), ("B".into(), 10), ("C".into(), 20)];
        app.pr_max_scroll = 40;

        app.jump_to_section(2);
        assert_eq!(app.pr_scroll, 20);
        app.jump_to_section(9);
        assert_eq!(app.pr_scroll, 20, "no tenth section to land on");
    }

    #[test]
    fn the_outline_highlights_the_section_being_read() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pr_sections = vec![("A".into(), 0), ("B".into(), 10)];

        app.pr_scroll = 0;
        assert_eq!(active_section(&app.pr_sections, app.pr_scroll), Some(0));
        app.pr_scroll = 9;
        assert_eq!(active_section(&app.pr_sections, app.pr_scroll), Some(0));
        app.pr_scroll = 10;
        assert_eq!(active_section(&app.pr_sections, app.pr_scroll), Some(1));
    }

    #[test]
    fn section_jumps_step_forward_and_land_on_the_current_heading_first() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.pr_sections = vec![("A".into(), 0), ("B".into(), 10), ("C".into(), 20)];
        app.pr_max_scroll = 40;

        app.jump_section(1);
        assert_eq!(app.pr_scroll, 10);
        app.jump_section(1);
        assert_eq!(app.pr_scroll, 20);
        app.jump_section(1);
        assert_eq!(app.pr_scroll, 20, "clamps at the last section");

        // Back from mid-section lands on this heading before the previous one.
        app.pr_scroll = 25;
        app.jump_section(-1);
        assert_eq!(app.pr_scroll, 20);
        app.jump_section(-1);
        assert_eq!(app.pr_scroll, 10);
    }

    #[test]
    fn clicking_the_outline_jumps_to_that_section() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        let mut pr = empty_pull_request("base");
        pr.body = (1..=40)
            .map(|i| format!("paragraph {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        app.pull_request = Some(pr);
        app.page = Page::PullRequest;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.pr_sections.len(), 2);
        assert!(app.pr_outline_area.width > 0, "wide page gets a rail");
        let second = app.pr_sections[1].1;
        assert!(second > 0 && app.pr_max_scroll > 0, "document overflows");

        // One padding row, then entry index 1.
        let (x, y) = (app.pr_outline_area.x + 1, app.pr_outline_area.y + 2);
        handle_mouse(&mut app, left_click(x, y));
        assert_eq!(app.pr_scroll, second.min(app.pr_max_scroll));
    }

    /// The rail is the discoverable half of section navigation, not the only
    /// half: a narrow page drops it and keeps `]` / `[`.
    #[test]
    fn a_narrow_pr_page_keeps_its_sections_without_a_rail() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(50, 24);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.pull_request = Some(empty_pull_request("base"));
        app.page = Page::PullRequest;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.pr_outline_area, Rect::default());
        assert_eq!(app.pr_sections.len(), 2);
    }

    #[test]
    fn help_scroll_is_clamped_to_a_small_terminal() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let terminal_backend = ratatui::backend::TestBackend::new(80, 12);
        let mut terminal = Terminal::new(terminal_backend).unwrap();
        app.show_help = true;

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.help_max_scroll > 0);

        app.help_scroll = u16::MAX;
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.help_scroll, app.help_max_scroll);
    }

    #[test]
    fn background_load_failure_stays_visible() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();
        backend.fail.store(true, Ordering::SeqCst);

        app.request_current_scope();
        settle_load(&mut app);

        assert_eq!(app.load_error.as_deref(), Some("synthetic load failure"));
    }

    #[test]
    fn multiline_load_failure_renders_its_complete_diagnostic() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.load_error = Some(
            "jj rejected the selected revision\nHint: choose the exact commit id\nfinal diagnostic line"
                .into(),
        );
        let terminal_backend = ratatui::backend::TestBackend::new(48, 12);
        let mut terminal = Terminal::new(terminal_backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("reload failed"));
        assert!(rendered.contains("jj rejected the selected revision"));
        assert!(rendered.contains("Hint: choose the exact commit id"));
        assert!(rendered.contains("final diagnostic line"));
    }

    #[test]
    fn restored_note_composer_reopens_with_its_text_and_caret() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-composer", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");
        let mut store = state::Store::load_at(&state_home, &root, None).unwrap();
        let composer = NoteDraft {
            kind: ComposerKind::AgentNote,
            anchor: Some(("src/main.rs".into(), 12)),
            body: "unfinished thought".into(),
            caret: 8,
            error: None,
            editing: None,
        };
        store.set_note_composer(Some(&composer));
        store.save().unwrap();

        let backend = Arc::new(TestBackend::new());
        let app = App::load(
            backend,
            Highlighter::new(),
            None,
            Some(state::Store::load_at(&state_home, &root, None).unwrap()),
        )
        .unwrap();
        assert_eq!(app.mode, Mode::NoteInput(composer));
    }

    #[test]
    fn composer_autosave_flushes_after_the_debounce() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-debounce", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(
            backend,
            Highlighter::new(),
            None,
            Some(state::Store::load_at(&state_home, &root, None).unwrap()),
        )
        .unwrap();
        app.mode = Mode::NoteInput(NoteDraft {
            kind: ComposerKind::AgentNote,
            anchor: Some(("src/main.rs".into(), 12)),
            body: "autosaved text".into(),
            caret: 9,
            error: None,
            editing: None,
        });
        app.persist_soon();
        app.persistence_due = Some(Instant::now());
        assert!(app.poll_persistence());

        let restored = state::Store::load_at(&state_home, &root, None).unwrap();
        assert_eq!(
            restored.notes().2.map(|draft| draft.body.as_str()),
            Some("autosaved text")
        );
    }

    #[test]
    fn attaching_a_pr_restores_only_its_saved_head_draft() {
        let root =
            std::env::temp_dir().join(format!("recto-app-state-{}-review", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let state_home = root.join("state");
        let mut store = state::Store::load_at(&state_home, &root, None).unwrap();
        let key = link::PullRequestRef {
            repository: "owner/repo".into(),
            number: 42,
            head_oid: "abc123".into(),
        };
        store.set_review(
            key,
            Some(&link::DraftReviewBody {
                body: "Saved overall review".into(),
                last_editor: link::DraftEditor::User,
            }),
            &[link::DraftReviewComment {
                id: 7,
                path: "src/main.rs".into(),
                start: 12,
                end: 12,
                body: "Saved inline comment".into(),
                last_editor: link::DraftEditor::Agent,
            }],
            8,
        );
        store.save().unwrap();

        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(
            backend,
            Highlighter::new(),
            None,
            Some(state::Store::load_at(&state_home, &root, None).unwrap()),
        )
        .unwrap();
        let response = app.attach_pull_request(empty_pull_request("base"), false);
        assert!(response.ok);
        assert_eq!(
            app.review_draft_body
                .as_ref()
                .map(|draft| draft.body.as_str()),
            Some("Saved overall review")
        );
        assert_eq!(app.review_draft_comments[0].id, 7);
        assert_eq!(app.next_review_draft_id, 8);
    }

    #[test]
    fn note_acknowledgement_removes_only_the_ids_that_were_read() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        app.agent_notes = vec![
            AgentNote {
                id: 4,
                path: "one".into(),
                start: 1,
                end: 1,
                body: "first".into(),
            },
            AgentNote {
                id: 5,
                path: "two".into(),
                start: 2,
                end: 2,
                body: "new arrival".into(),
            },
        ];

        let response = app.acknowledge_agent_notes(&[4]);
        assert!(response.ok);
        assert_eq!(app.agent_notes.len(), 1);
        assert_eq!(app.agent_notes[0].id, 5);
        assert!(app.revise_agent_note(5, "still the new arrival".into()).ok);
        assert_eq!(app.agent_notes[0].body, "still the new arrival");
    }

    #[test]
    fn attaching_pull_request_selects_its_exact_base() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let response = app.handle_request(link::Request::AttachPr {
            pull_request: Box::new(empty_pull_request("stack-base")),
        });

        assert!(response.ok);
        assert_eq!(app.base(), &Base::Revision("stack-base".into()));
        assert!(matches!(app.page, Page::PullRequest));
        assert!(app.loading.is_some());
    }

    #[test]
    fn hiding_comments_leaves_tour_annotations_woven() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend, Highlighter::new(), None, None).unwrap();
        let changes = vec![change("foo.go")];
        let fetch: Box<FetchContent> = Box::new(|_| None);
        let rendered = render_diff(TWO_HUNK_DIFF, &changes, &Highlighter::new(), &*fetch);
        app.apply_loaded(
            Scope::Range(Base::Revision("base".into())),
            LoadedDiff {
                workspace_revision: "abc123".into(),
                changes,
                rendered: rendered.lines,
                file_starts: rendered.file_starts,
                line_info: rendered.line_info,
                file_stats: rendered.file_stats,
                revs: Some(Vec::new()),
            },
        );
        app.annotations.push(Annotation {
            path: "foo.go".into(),
            start: 2,
            end: 2,
            label: "Tour stop".into(),
        });
        app.review_draft_comments.push(link::DraftReviewComment {
            id: 7,
            path: "foo.go".into(),
            start: 2,
            end: 2,
            body: "Shared draft".into(),
            last_editor: link::DraftEditor::User,
        });
        app.agent_notes.push(AgentNote {
            id: 8,
            path: "foo.go".into(),
            start: 2,
            end: 2,
            body: "Private note".into(),
        });
        let mut pr = empty_pull_request("base");
        pr.threads.push(link::ReviewThread {
            id: "thread-1".into(),
            path: "foo.go".into(),
            side: link::DiffSide::Right,
            line: Some(2),
            start_line: None,
            original_line: Some(2),
            original_start_line: None,
            resolved: false,
            outdated: false,
            comments: Vec::new(),
        });
        app.pull_request = Some(pr);
        app.reweave();
        assert_eq!(app.rendered_review_objects.iter().flatten().count(), 4);

        let response = app.handle_request(link::Request::CommentVisibility {
            visible: Some(false),
        });
        assert!(response.ok);
        assert!(!app.show_comments);
        assert_eq!(app.annotations.len(), 1);
        assert_eq!(
            app.rendered_review_objects
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>(),
            vec![FileReviewObject::TourStop(0)]
        );
        assert_eq!(
            app.file_rows
                .iter()
                .filter(|row| matches!(row, FileRow::ReviewObject { .. }))
                .count(),
            1
        );
        assert!(
            !app.handle_request(link::Request::Ping)
                .status
                .expect("ping status")
                .comments_visible
        );

        app.handle_request(link::Request::CommentVisibility { visible: None });
        assert!(app.show_comments);
        assert_eq!(app.rendered_review_objects.iter().flatten().count(), 4);
    }

    #[test]
    fn stale_review_reports_both_heads_and_refuses_agent_targets() {
        let backend = Arc::new(TestBackend::new());
        let mut app = App::load(backend.clone(), Highlighter::new(), None, None).unwrap();
        assert!(
            app.attach_pull_request(empty_pull_request("base"), false)
                .ok
        );
        app.focus_span = Some(FocusSpan {
            path: "src/main.rs".into(),
            start: 12,
            end: 12,
            set_at: Instant::now(),
        });
        app.annotations.push(Annotation {
            path: "src/main.rs".into(),
            start: 12,
            end: 12,
            label: "old tour".into(),
        });

        backend.set_revision("def456");
        let live_status = app
            .handle_request(link::Request::Ping)
            .status
            .expect("live ping status");
        assert_eq!(live_status.workspace_revision, "def456");
        assert!(live_status.stale_review);
        let live_focus = app.handle_request(link::Request::Focus {
            path: "src/main.rs".into(),
            start: Some(12),
            end: None,
        });
        assert!(!live_focus.ok, "live mismatch must fail before a reload");

        let moved = load_diff(&*backend, &Highlighter::new(), &test_request(1)).unwrap();
        app.apply_loaded(Scope::Range(Base::Revision("base".into())), moved);

        assert!(app.focus_span.is_none());
        assert!(app.annotations.is_empty());
        let response = app.handle_request(link::Request::Ping);
        let status = response.status.expect("ping status");
        assert_eq!(status.workspace_revision, "def456");
        assert_eq!(status.pull_request.expect("attached PR").head_oid, "abc123");
        assert!(status.stale_review);

        let focus = app.handle_request(link::Request::Focus {
            path: "src/main.rs".into(),
            start: Some(12),
            end: None,
        });
        assert!(!focus.ok);
        assert!(focus.error.as_deref().is_some_and(|error| {
            error.contains("attached head abc123") && error.contains("workspace revision def456")
        }));
        let annotate = app.handle_request(link::Request::Annotate {
            sites: vec![link::Site {
                path: "src/main.rs".into(),
                start: 12,
                end: None,
                label: "new tour".into(),
            }],
        });
        assert!(!annotate.ok);
        assert!(app.annotations.is_empty());
    }

    const GO_SAMPLE: &str = r#"package x

func extractHTTPPort(spec *Sandbox) (int64, bool) {
    a := 1
    b := 2
    c := 3
    d := 4
    e := 5
    f := 6
    g := 7
    h := 8
    return int64(a + b + c + d + e + f + g + h), true
}
"#;

    #[test]
    fn augment_fills_empty_go_header() {
        let header = "@@ -3,11 +3,11 @@";
        let out = augment_hunk_header(header, "go", Some(GO_SAMPLE), 7);
        assert_eq!(
            out,
            "@@ -3,11 +3,11 @@ func extractHTTPPort(spec *Sandbox) (int64, bool) {"
        );
    }

    #[test]
    fn augment_preserves_existing_context() {
        let header = "@@ -3,11 +3,11 @@ already there";
        let out = augment_hunk_header(header, "go", Some(GO_SAMPLE), 7);
        assert_eq!(out, header);
    }

    #[test]
    fn augment_noop_for_unknown_ext() {
        let header = "@@ -3,11 +3,11 @@";
        let out = augment_hunk_header(header, "rs", Some(GO_SAMPLE), 7);
        assert_eq!(out, header);
    }

    #[test]
    fn augment_noop_when_content_unavailable() {
        let header = "@@ -3,11 +3,11 @@";
        let out = augment_hunk_header(header, "go", None, 7);
        assert_eq!(out, header);
    }

    #[test]
    fn only_current_new_side_review_threads_anchor_in_the_diff() {
        let mut thread = link::ReviewThread {
            id: "thread-1".into(),
            path: "src/main.rs".into(),
            side: link::DiffSide::Right,
            line: Some(45),
            start_line: Some(42),
            original_line: Some(40),
            original_start_line: Some(38),
            resolved: false,
            outdated: false,
            comments: Vec::new(),
        };
        assert_eq!(review_thread_span(&thread), Some((42, 45)));

        thread.side = link::DiffSide::Left;
        assert_eq!(review_thread_span(&thread), None);
        thread.side = link::DiffSide::Right;
        thread.outdated = true;
        assert_eq!(review_thread_span(&thread), None);
    }

    #[test]
    fn display_row_index_maps_both_directions() {
        let lines = vec![
            Line::from("one"),
            Line::from("alpha beta gamma"),
            Line::from("last"),
        ];
        let index = DisplayRowIndex::build(&lines, &[None, None, None], 5);

        assert_eq!(index.starts, vec![0, 1, 4, 5]);
        assert_eq!(index.total_rows(), 5);
        assert_eq!(index.row_of_line(1), 1);
        assert_eq!(index.line_at_row(0), Some((0, 0)));
        assert_eq!(index.line_at_row(1), Some((1, 0)));
        assert_eq!(index.line_at_row(3), Some((1, 2)));
        assert_eq!(index.line_at_row(4), Some((2, 0)));
    }

    #[test]
    fn display_row_index_counts_note_continuations() {
        let line = note_line(1, "alpha beta gamma delta epsilon!");
        let expected = wrap::wrap_line(&line, 18, &wrap::note_prefix(&line)).len();
        let index = DisplayRowIndex::build(&[line], &[None], 18);

        assert!(expected > 1);
        assert_eq!(index.total_rows(), expected);
    }

    #[test]
    fn display_row_index_clamps_past_the_end() {
        let lines = vec![Line::from("one"), Line::from("two")];
        let index = DisplayRowIndex::build(&lines, &[None, None], 80);

        assert_eq!(index.row_of_line(99), 2);
        assert_eq!(index.line_at_row(99), Some((1, 0)));
        assert_eq!(DisplayRowIndex::default().line_at_row(0), None);
    }

    // file 0: header (None), lines 10,11,12 ; file 1: header (None), line 5
    fn sample_line_info() -> Vec<LineInfo> {
        vec![
            None,
            Some((0, 10)),
            Some((0, 11)),
            Some((0, 12)),
            None,
            Some((1, 5)),
        ]
    }

    fn draft() -> NoteDraft {
        NoteDraft {
            kind: ComposerKind::AgentNote,
            anchor: Some(("src/main.rs".into(), 42)),
            body: String::new(),
            caret: 0,
            error: None,
            editing: None,
        }
    }

    /// The caret indexes characters, not bytes, so editing after a multi-byte
    /// character has to keep landing on character boundaries.
    #[test]
    fn draft_edits_by_character_not_byte() {
        let mut d = draft();
        for c in "héllo".chars() {
            d.insert(c);
        }
        assert_eq!(d.body, "héllo");
        assert_eq!(d.caret, 5);

        // Back up over the multi-byte char and insert in front of it.
        d.caret = 1;
        d.insert('x');
        assert_eq!(d.body, "hxéllo");
        assert_eq!(d.caret, 2);

        d.backspace();
        assert_eq!(d.body, "héllo");
        assert_eq!(d.caret, 1);

        // Backspacing at the start is a no-op rather than a panic.
        d.caret = 0;
        d.backspace();
        assert_eq!(d.body, "héllo");
    }

    /// The modal places the terminal caret from this, so a body with newlines
    /// has to report the row and the column within that row.
    #[test]
    fn draft_caret_tracks_rows_and_columns() {
        let mut d = draft();
        for c in "one".chars() {
            d.insert(c);
        }
        let rc = |d: &NoteDraft| d.caret_rc(&d.wrap_rows(80));
        assert_eq!(rc(&d), (0, 3));
        d.insert('\n');
        assert_eq!(rc(&d), (1, 0));
        for c in "two".chars() {
            d.insert(c);
        }
        assert_eq!(rc(&d), (1, 3));
        assert_eq!(d.body, "one\ntwo");
        // Caret back up on the first row reports that row's column.
        d.caret = 1;
        assert_eq!(rc(&d), (0, 1));
    }

    #[test]
    fn draft_mouse_click_maps_through_the_visible_wrapped_rows() {
        let mut d = draft();
        d.body = "abcdefghijklmno".into();
        let layout = NoteLayout {
            body: Rect::new(10, 5, 6, 2),
            wrap_width: 5,
            first_row: 1,
        };

        // The second visible row is the third wrapped row in the body.
        move_note_caret_to_click(&mut d, layout, Position { x: 12, y: 6 });
        assert_eq!(d.caret, 12);

        // Blank space after a short line clamps to that row's end.
        move_note_caret_to_click(&mut d, layout, Position { x: 15, y: 5 });
        assert_eq!(d.caret, 10);

        // Borders and the rest of the screen do not retarget the composer.
        move_note_caret_to_click(&mut d, layout, Position { x: 9, y: 5 });
        assert_eq!(d.caret, 10);
    }

    /// Long prose soft-wraps at word boundaries instead of running off the
    /// edge, which is the whole reason the modal grew a layout pass.
    #[test]
    fn draft_soft_wraps_at_word_boundaries() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        let rows = d.wrap_rows(10);
        let text = |r: &Range<usize>| {
            d.body.chars().collect::<Vec<_>>()[r.clone()]
                .iter()
                .collect::<String>()
        };
        assert_eq!(
            rows.iter().map(text).collect::<Vec<_>>(),
            vec!["the quick ", "brown fox"]
        );

        // A word with no break point in it gets cut at the edge rather than
        // vanishing past the border.
        d.body = "supercalifragilistic".into();
        assert_eq!(d.wrap_rows(10), vec![0..10, 10..20]);
    }

    /// `ctrl-u` and `ctrl-k` span the whole note, not the visual row the caret
    /// happens to sit on — clearing a wrapped sentence should clear all of it.
    #[test]
    fn draft_kill_verbs_span_the_logical_line() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        d.caret = 10;
        d.cut(d.line_bounds().start..d.caret);
        assert_eq!(d.body, "brown fox");
        assert_eq!(d.caret, 0);

        d.caret = 6;
        d.cut(d.caret..d.line_bounds().end);
        assert_eq!(d.body, "brown ");
        assert_eq!(d.caret, 6);

        // On a multi-line note the verbs stop at the newline rather than
        // eating the neighbouring line.
        d.body = "one\ntwo\nthree".into();
        d.caret = 6;
        d.cut(d.line_bounds().start..d.caret);
        assert_eq!(d.body, "one\no\nthree");
    }

    /// Word motion skips the separators then the word, so repeated `alt-b`
    /// walks backwards a word at a time instead of stalling on punctuation.
    #[test]
    fn draft_word_motion_walks_over_separators() {
        let mut d = draft();
        d.body = "fix the off-by-one".into();
        d.caret = d.len();
        d.caret = d.prev_word();
        assert_eq!(d.caret, 15); // "one"
        d.caret = d.prev_word();
        assert_eq!(d.caret, 12); // "by"
        d.caret = d.prev_word();
        assert_eq!(d.caret, 8); // "off"
        d.caret = d.prev_word();
        assert_eq!(d.caret, 4); // "the"

        d.caret = 0;
        d.caret = d.next_word();
        assert_eq!(d.caret, 3);
        d.caret = d.next_word();
        assert_eq!(d.caret, 7);

        // ctrl-w cuts back to the word start and leaves the caret in the hole.
        d.caret = d.len();
        d.cut(d.prev_word()..d.caret);
        assert_eq!(d.body, "fix the off-by-");
        assert_eq!(d.caret, 15);
    }

    /// Up and down move by wrapped row, so a long note is navigable by what's
    /// on screen even though it's a single logical line.
    #[test]
    fn draft_vertical_motion_follows_wrapped_rows() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        let rows = d.wrap_rows(10);

        d.caret = 12; // row 1, column 2
        d.move_row(&rows, -1);
        assert_eq!(d.caret_rc(&rows), (0, 2));
        d.move_row(&rows, 1);
        assert_eq!(d.caret_rc(&rows), (1, 2));

        // Off either end is a no-op rather than a wrap-around or a panic.
        d.move_row(&rows, 1);
        assert_eq!(d.caret_rc(&rows), (1, 2));
        d.caret = 2;
        d.move_row(&rows, -1);
        assert_eq!(d.caret_rc(&rows), (0, 2));

        // Dropping onto a shorter row clamps to its end.
        d.body = "a longer first row\nhi".into();
        let rows = d.wrap_rows(40);
        d.caret = 10;
        d.move_row(&rows, 1);
        assert_eq!(d.caret, d.len());
    }

    /// Forward delete takes the character under the caret and is a no-op at
    /// the end, where `backspace` would otherwise be the only way out.
    #[test]
    fn draft_forward_delete_stops_at_the_end() {
        let mut d = draft();
        d.body = "héllo".into();
        d.caret = 1;
        d.delete();
        assert_eq!(d.body, "hllo");
        assert_eq!(d.caret, 1);

        d.caret = d.len();
        d.delete();
        assert_eq!(d.body, "hllo");
    }

    /// Every caret position has to land on exactly one row, including the two
    /// ambiguous ones: the soft-wrap seam and the newline.
    #[test]
    fn draft_caret_resolves_wrap_seams() {
        let mut d = draft();
        d.body = "the quick brown fox".into();
        let rows = d.wrap_rows(10);

        // Mid-word is unambiguous.
        d.caret = 4;
        assert_eq!(d.caret_rc(&rows), (0, 4));

        // The seam: rows touch, so the caret rides the continuation row rather
        // than sitting in the column the border occupies.
        d.caret = 10;
        assert_eq!(d.caret_rc(&rows), (1, 0));

        // End of body stays on the last row.
        d.caret = d.len();
        assert_eq!(d.caret_rc(&rows), (1, 9));

        // A newline leaves a one-character gap between rows, so the caret sits
        // at the end of the earlier row instead of jumping to the next.
        d.body = "one\ntwo".into();
        let rows = d.wrap_rows(10);
        d.caret = 3;
        assert_eq!(d.caret_rc(&rows), (0, 3));
        d.caret = 4;
        assert_eq!(d.caret_rc(&rows), (1, 0));
    }

    /// An empty body still has a row for the caret to sit on, and a trailing
    /// newline opens a fresh one.
    #[test]
    fn draft_wrap_always_yields_a_caret_row() {
        let mut d = draft();
        assert_eq!(d.wrap_rows(10), vec![0..0]);
        assert_eq!(d.caret_rc(&d.wrap_rows(10)), (0, 0));

        d.body = "hi\n".into();
        d.caret = 3;
        let rows = d.wrap_rows(10);
        assert_eq!(rows, vec![0..2, 3..3]);
        assert_eq!(d.caret_rc(&rows), (1, 0));
    }

    /// `c` anywhere inside a note's span re-opens that note, so a comment
    /// pinned to 10-14 is reachable from line 12 and not just line 10.
    #[test]
    fn comment_lookup_covers_the_whole_span() {
        let comments = vec![
            AgentNote {
                id: 1,
                path: "src/main.rs".into(),
                start: 10,
                end: 14,
                body: "range note".into(),
            },
            AgentNote {
                id: 2,
                path: "src/link.rs".into(),
                start: 3,
                end: 3,
                body: "single".into(),
            },
        ];
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 10), Some(0));
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 12), Some(0));
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 14), Some(0));
        assert_eq!(agent_note_index_at(&comments, "src/link.rs", 3), Some(1));
        // Just outside the span, and the right line in the wrong file.
        assert_eq!(agent_note_index_at(&comments, "src/main.rs", 15), None);
        assert_eq!(agent_note_index_at(&comments, "src/link.rs", 12), None);
    }

    /// The cursor steps over rows with no line info, so a hunk header or a
    /// woven note between two code lines costs no keypresses to cross.
    #[test]
    fn cursor_steps_skip_unpointable_rows() {
        let info = sample_line_info();
        // Row 0 has no info; from row 1 a single step lands on row 2.
        assert_eq!(step_pointable(&info, 1, 1), Some(2));
        // Row 4 has no info, so stepping off row 3 crosses it to row 5.
        assert_eq!(step_pointable(&info, 3, 1), Some(5));
        assert_eq!(step_pointable(&info, 5, -1), Some(3));
        // Row 0 is unpointable, so walking back from row 1 finds nothing.
        assert_eq!(step_pointable(&info, 1, -1), None);
    }

    /// A step longer than the diff clamps to the last reachable row instead of
    /// refusing to move, so a fast repeat never sticks partway.
    #[test]
    fn cursor_steps_clamp_at_the_ends() {
        let info = sample_line_info();
        assert_eq!(step_pointable(&info, 1, 99), Some(5));
        assert_eq!(step_pointable(&info, 5, -99), Some(1));
        assert_eq!(step_pointable(&info, 5, 1), None);
    }

    #[test]
    fn span_rows_intersecting_range() {
        // Request 11-12 in file 0 → rendered rows 2..=3.
        assert_eq!(rows_for_span(&sample_line_info(), 0, 11, 12), Some(2..=3));
    }

    #[test]
    fn span_rows_clamps_to_shown_lines() {
        // Request 12-99 in file 0; only line 12 (row 3) is shown.
        assert_eq!(rows_for_span(&sample_line_info(), 0, 12, 99), Some(3..=3));
    }

    #[test]
    fn span_rows_none_when_outside_hunk() {
        // Lines 50-60 of file 0 aren't in the diff at all.
        assert_eq!(rows_for_span(&sample_line_info(), 0, 50, 60), None);
    }

    /// The snippet reader recovers a row's diff sign from which line-number
    /// columns are populated, and quotes the new-side number only. Removed rows
    /// have no new-side number, which is what tells them apart from context.
    #[test]
    fn gutter_signature_reads_each_side() {
        let added = body_row("+    let b = 3;", None, Some(42));
        assert_eq!(gutter_signature(&added), Some(('+', Some(42))));
        assert_eq!(body_text(&added), "    let b = 3;");

        let removed = body_row("-    let b = 2;", Some(41), None);
        assert_eq!(gutter_signature(&removed), Some(('-', None)));
        assert_eq!(body_text(&removed), "    let b = 2;");

        let context = body_row(" fn main() {", Some(40), Some(40));
        assert_eq!(gutter_signature(&context), Some((' ', Some(40))));
        assert_eq!(body_text(&context), "fn main() {");
    }

    /// Rows that aren't diff bodies must not be quoted into a snippet. A woven
    /// note row is the dangerous one: it sits inside the diff stream and would
    /// otherwise echo an agent's own annotation back to it as if it were code.
    #[test]
    fn gutter_signature_rejects_non_body_rows() {
        assert_eq!(gutter_signature(&note_line(1, "Step 1: parse")), None);
        assert_eq!(gutter_signature(&agent_note_line(1, "why 3?", true)), None);
        assert_eq!(gutter_signature(&hunk_header("@@ -1,3 +1,4 @@")), None);
    }

    #[test]
    fn hunk_starts_ignore_section_heading() {
        // A heading whose code contains a `-1` token must not clobber the
        // real start lines, which sit right after the opening `@@`.
        assert_eq!(
            parse_hunk_starts("@@ -604,6 +607,175 @@ func f() { return -1 }"),
            Some((604, 607))
        );
    }

    #[test]
    fn second_hunk_reseeds_line_numbers() {
        let hl = Highlighter::new();
        let changes = vec![FileChange {
            path: "foo.go".into(),
            status: FileStatus::Modified,
        }];
        let fetch: Box<FetchContent> = Box::new(|_| None);
        let rd = render_diff(TWO_HUNK_DIFF, &changes, &hl, &*fetch);

        // The added line in the second hunk is new-side line 111. With the
        // bug it lands around line 6 (counter never jumped to 110), so a focus
        // request for 111 resolves to nothing.
        assert!(
            rows_for_span(&rd.line_info, 0, 111, 111).is_some(),
            "second-hunk line 111 should be focusable; line_info = {:?}",
            rd.line_info
        );
        // And the whole second hunk should carry 110..=113, not 5..=8.
        assert!(
            rd.line_info.contains(&Some((0, 110))) && rd.line_info.contains(&Some((0, 113))),
            "second hunk should be numbered 110..=113; line_info = {:?}",
            rd.line_info
        );
    }
}
