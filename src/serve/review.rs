//! Review drafts with no viewer running. The web window shows a review rig's
//! drafts beside its diff, so with no terminal Recto listening, the review
//! commands write the durable store directly, the way a tour does, and answer
//! with the same draft a live viewer would. The rules match the viewer's: a
//! draft needs an attached pull request, an inline comment has to land on a
//! line the PR's diff shows, and a write against a stale revision is refused.

use super::*;
use crate::link::{DraftEditor, DraftReviewBody, DraftReviewComment, PullRequestRef, ReviewDraft};

/// The answer to a review request, or None for a request this doesn't handle.
pub(crate) fn respond_offline(request: &link::Request) -> Option<link::Response> {
    use link::Request::*;
    if !matches!(
        request,
        AttachPr { .. }
            | ReviewDraft
            | ReviewVerdict { .. }
            | ReviewDraftBody { .. }
            | ReviewDraftComment { .. }
    ) {
        return None;
    }
    Some(respond(request).unwrap_or_else(|e| link::Response::err(format!("{e:#}"))))
}

/// `recto review-focus` with no viewer. The web window follows focus, and a
/// draft sits under the lines it's about, so focusing those lines puts the
/// draft in front of the reader. The summary needs nothing: it heads the
/// repo already. Returns the process exit code.
pub(crate) fn focus_offline(id: Option<u64>) -> i32 {
    let Some(id) = id else {
        eprintln!(
            "recto: no viewer running; the web window shows the summary on the PR's own page"
        );
        return 0;
    };
    let draft = match respond(&link::Request::ReviewDraft) {
        Ok(resp) if resp.ok => resp.review_draft,
        Ok(resp) => {
            eprintln!("recto: {}", resp.error.unwrap_or_default());
            return 1;
        }
        Err(e) => {
            eprintln!("recto: {e:#}");
            return 1;
        }
    };
    match draft.and_then(|d| d.comments.into_iter().find(|c| c.id == id)) {
        Some(c) => super::save_focus_offline(&c.path, Some(c.start), Some(c.end)),
        None => {
            eprintln!("recto: no draft {id} in this review");
            1
        }
    }
}

struct Slate {
    store: state::Store,
    pr: PullRequestRef,
    verdict: Option<link::Verdict>,
    body: Option<DraftReviewBody>,
    comments: Vec<DraftReviewComment>,
    next_id: u64,
    files: Vec<FileDiff>,
}

impl Slate {
    fn draft(&self) -> ReviewDraft {
        ReviewDraft {
            pull_request: self.pr.clone(),
            verdict: self.verdict,
            body: self.body.clone(),
            comments: self
                .comments
                .iter()
                .map(|c| DraftReviewComment {
                    snippet: c
                        .pin
                        .shown()
                        .then(|| snippet(&self.files, &c.path, c.start, c.end))
                        .flatten(),
                    ..c.clone()
                })
                .collect(),
        }
    }

    fn save(mut self) -> Result<link::Response> {
        self.store.set_review(
            self.pr.clone(),
            self.verdict,
            self.body.as_ref(),
            &self.comments,
            self.next_id,
        );
        self.store.save()?;
        Ok(link::Response::ok_review_draft(self.draft()))
    }

    fn conflict(&self, what: &str, expected: u64, current: u64) -> Option<link::Response> {
        (expected != current).then(|| {
            link::Response::draft_conflict(
                format!(
                    "{what} is at revision {current}, not {expected}: it changed since that read, \
                     so start from the current body"
                ),
                self.draft(),
            )
        })
    }
}

fn respond(request: &link::Request) -> Result<link::Response> {
    let backend = crate::backend::detect_backend()?;
    let store = state::Store::load(backend.root(), None)?;
    apply(backend.as_ref(), store, request)
}

fn apply(
    backend: &dyn Backend,
    mut store: state::Store,
    request: &link::Request,
) -> Result<link::Response> {
    if let link::Request::AttachPr { pull_request } = request {
        store.set_pull_request(Some(pull_request));
        store.save()?;
        return Ok(link::Response::ok_note(format!(
            "attached {}#{} for the web window (no Recto viewer is running)",
            pull_request.repository, pull_request.number
        )));
    }
    let Some(pr) = store.pull_request().cloned() else {
        return Ok(link::Response::err(
            "attach a pull request before drafting a review (recto pr URL)",
        ));
    };
    let pr_ref = PullRequestRef {
        repository: pr.repository.clone(),
        number: pr.number,
        head_oid: pr.head_oid.clone(),
    };
    let restored = store.review(&pr_ref);
    // The PR's own range, which is what GitHub compares and what the drafts'
    // line numbers mean.
    let base = view_base(backend, Some(&pr))?;
    let files = parse_diff(&backend.unified_diff(&Scope::Range(base), false)?);
    let mut slate = Slate {
        store,
        pr: pr_ref,
        verdict: restored.as_ref().and_then(|r| r.verdict),
        body: restored.as_ref().and_then(|r| r.body.clone()),
        comments: restored
            .as_ref()
            .map(|r| r.comments.clone())
            .unwrap_or_default(),
        next_id: restored.as_ref().map_or(1, |r| r.next_id),
        files,
    };
    crate::anchor::settle(
        backend,
        &crate::anchor::Lens::range(backend),
        &mut slate.comments,
    );
    slate_order(&slate.files, &mut slate.comments);
    match request {
        link::Request::ReviewDraft => Ok(link::Response::ok_review_draft(slate.draft())),
        link::Request::ReviewVerdict { verdict } => {
            slate.verdict = *verdict;
            slate.save()
        }
        link::Request::ReviewDraftBody { body, if_revision } => {
            let current = slate.body.as_ref().map_or(0, |b| b.revision);
            if let Some(c) = if_revision.and_then(|e| slate.conflict("the summary", e, current)) {
                return Ok(c);
            }
            let body = body.trim().to_string();
            slate.body = (!body.is_empty()).then_some(DraftReviewBody {
                body,
                last_editor: DraftEditor::Agent,
                revision: current + 1,
            });
            slate.save()
        }
        link::Request::ReviewDraftComment {
            id: Some(id),
            body,
            if_revision,
            ..
        } => {
            let Some(i) = slate.comments.iter().position(|c| c.id == *id) else {
                return Ok(link::Response::err(format!(
                    "review draft comment {id} does not exist"
                )));
            };
            let current = slate.comments[i].revision;
            if let Some(c) =
                if_revision.and_then(|e| slate.conflict(&format!("comment {id}"), e, current))
            {
                return Ok(c);
            }
            let body = body.trim().to_string();
            if body.is_empty() {
                slate.comments.remove(i);
            } else {
                let c = &mut slate.comments[i];
                c.body = body;
                c.last_editor = DraftEditor::Agent;
                c.revision = current + 1;
            }
            slate.save()
        }
        link::Request::ReviewDraftComment {
            id: None,
            path,
            start,
            end,
            body,
            ..
        } => {
            let (Some(path), Some(start)) = (path.as_deref(), *start) else {
                return Ok(link::Response::err(
                    "a new review comment needs path:LINE=body",
                ));
            };
            let body = body.trim().to_string();
            if body.is_empty() {
                return Ok(link::Response::err("review comment body is empty"));
            }
            let end = end.unwrap_or(start).max(start);
            let Some(file) = slate.files.iter().find(|f| f.path == path) else {
                return Ok(link::Response::err(format!("not in current diff: {path}")));
            };
            if !shows_span(file, start, end) {
                return Ok(link::Response::err(format!(
                    "{path}:{start}-{end} not in current diff (outside any shown hunk)"
                )));
            }
            let id = slate.next_id;
            slate.next_id += 1;
            slate.comments.push(DraftReviewComment {
                id,
                path: path.to_string(),
                start,
                end,
                body,
                last_editor: DraftEditor::Agent,
                revision: 1,
                snippet: None,
                pin: crate::anchor::Pin::stamped(crate::anchor::Resolver::new(backend).stamp(
                    &crate::anchor::Lens::range(backend),
                    path,
                    start,
                    end,
                )),
            });
            slate_order(&slate.files, &mut slate.comments);
            slate.save()
        }
        _ => unreachable!("filtered in respond_offline"),
    }
}

/// Put review drafts in slate order, the order the Review tab and `recto
/// review` number them in (✎1, ✎2, ...) and GitHub lays them out: drafts
/// still on the diff first, by the diff's file order, then span, then id.
/// Matches the viewer's own sort, so every surface gives a draft one number.
pub(super) fn slate_order(files: &[FileDiff], comments: &mut [DraftReviewComment]) {
    comments.sort_by_cached_key(|c| {
        let file = files.iter().position(|f| f.path == c.path);
        let anchored = c.pin.shown() && file.is_some_and(|i| shows_any(&files[i], c.start, c.end));
        (
            !anchored,
            file.unwrap_or(usize::MAX),
            c.path.clone(),
            c.start,
            c.end,
            c.id,
        )
    });
}

/// Whether any line of a new-side span shows in the diff. Looser than
/// `shows_span`, and the test the viewer sorts by (`rows_for_span`), so a draft
/// straddling two hunks still numbers the same on every surface.
fn shows_any(file: &FileDiff, start: u32, end: u32) -> bool {
    file.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter_map(|l| l.new)
        .any(|n| start <= n && n <= end)
}

/// Whether a new-side span lands inside one of the file's hunks, which is
/// what GitHub will accept a review comment on.
pub(super) fn shows_span(file: &FileDiff, start: u32, end: u32) -> bool {
    file.hunks.iter().any(|h| {
        let lines: Vec<u32> = h.lines.iter().filter_map(|l| l.new).collect();
        matches!((lines.first(), lines.last()), (Some(&lo), Some(&hi)) if lo <= start && end <= hi)
    })
}

/// The rows a draft points at with a line of context either side, the cut a
/// companion reads to know what it said something about. None once the span
/// no longer resolves in this diff.
fn snippet(files: &[FileDiff], path: &str, start: u32, end: u32) -> Option<Vec<link::SnippetRow>> {
    let file = files.iter().find(|f| f.path == path)?;
    let hunk = file.hunks.iter().find(|h| {
        let lines: Vec<u32> = h.lines.iter().filter_map(|l| l.new).collect();
        matches!((lines.first(), lines.last()), (Some(&lo), Some(&hi)) if lo <= start && end <= hi)
    })?;
    let lines: Vec<&DiffLine> = hunk
        .lines
        .iter()
        .filter(|l| !matches!(l.kind, Kind::Meta))
        .collect();
    let first = lines
        .iter()
        .position(|l| l.new.is_some_and(|n| n >= start))?;
    let last = lines
        .iter()
        .rposition(|l| l.new.is_some_and(|n| n <= end))?;
    let from = first.saturating_sub(1);
    let to = (last + 1).min(lines.len() - 1);
    let rows = lines[from..=to]
        .iter()
        .enumerate()
        .map(|(i, l)| link::SnippetRow {
            line: l.new.or(l.old),
            sign: match l.kind {
                Kind::Add => '+',
                Kind::Del => '-',
                _ => ' ',
            },
            text: l.text.clone(),
            commented: (first..=last).contains(&(from + i)),
        })
        .collect();
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo whose PR changes lines 3-4 of a.txt, with a store (in its own
    /// state directory) that has the PR attached.
    fn review_repo() -> (PathBuf, PathBuf) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("recto-review-{}-{nonce}", std::process::id()));
        let repo = dir.join("repo");
        let home = dir.join("state");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(
            repo.join("a.txt"),
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n",
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(
            repo.join("a.txt"),
            "1\n2\nthree\nfour\n5\n6\n7\n8\n9\n10\n11\n12\n",
        )
        .unwrap();
        git(&repo, &["commit", "-qam", "change"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let mut pr = crate::testing::empty_pull_request(&base);
        pr.head_oid = head;
        let mut store = state::Store::load_at(&home, &repo, None).unwrap();
        store.set_pull_request(Some(&pr));
        store.save().unwrap();
        (repo, home)
    }

    fn run(repo: &Path, home: &Path, request: link::Request) -> link::Response {
        let backend = crate::backend::detect_backend_from(repo).expect("a git repo");
        let store = state::Store::load_at(home, repo, None).unwrap();
        apply(backend.as_ref(), store, &request).unwrap()
    }

    fn add(path: &str, start: u32, end: Option<u32>, body: &str) -> link::Request {
        link::Request::ReviewDraftComment {
            id: None,
            path: Some(path.into()),
            start: Some(start),
            end,
            body: body.into(),
            if_revision: None,
        }
    }

    // With no viewer running, drafts are written and read back the way a
    // viewer would answer: on lines the PR shows, with a snippet, revisions
    // guarding against a stale writer, and an empty body deleting.
    #[test]
    fn drafts_are_written_without_a_viewer() {
        let (repo, home) = review_repo();
        let resp = run(
            &repo,
            &home,
            add("a.txt", 3, Some(4), "Why spell these out?"),
        );
        assert!(resp.ok, "{:?}", resp.error);
        let draft = resp.review_draft.unwrap();
        let c = &draft.comments[0];
        assert_eq!(
            (c.id, c.revision, c.last_editor),
            (1, 1, DraftEditor::Agent)
        );
        let snip = c
            .snippet
            .as_ref()
            .expect("the span resolves, so it has a snippet");
        assert!(
            snip.iter().any(|r| r.commented && r.text == "three"),
            "{snip:?}"
        );

        // Far outside any hunk, and a file the PR doesn't touch.
        assert!(!run(&repo, &home, add("a.txt", 12, None, "x")).ok);
        assert!(!run(&repo, &home, add("b.txt", 1, None, "x")).ok);

        let revise = |if_revision, body: &str| link::Request::ReviewDraftComment {
            id: Some(1),
            path: None,
            start: None,
            end: None,
            body: body.into(),
            if_revision,
        };
        let stale = run(&repo, &home, revise(Some(7), "nope"));
        assert!(
            !stale.ok && stale.review_draft.is_some(),
            "a conflict carries the draft"
        );
        let ok = run(
            &repo,
            &home,
            revise(Some(1), "Spelled-out numbers read oddly here."),
        );
        assert_eq!(ok.review_draft.unwrap().comments[0].revision, 2);

        let body = run(
            &repo,
            &home,
            link::Request::ReviewDraftBody {
                body: "Small, looks fine.".into(),
                if_revision: None,
            },
        );
        assert_eq!(body.review_draft.unwrap().body.unwrap().revision, 1);
        let verdict = run(
            &repo,
            &home,
            link::Request::ReviewVerdict {
                verdict: Some(link::Verdict::Approve),
            },
        );
        assert_eq!(
            verdict.review_draft.unwrap().verdict,
            Some(link::Verdict::Approve)
        );

        let gone = run(&repo, &home, revise(None, ""));
        assert!(gone.review_draft.unwrap().comments.is_empty());
        let _ = std::fs::remove_dir_all(repo.parent().unwrap());
    }

    #[test]
    fn a_draft_needs_an_attached_pull_request() {
        let (repo, home) = review_repo();
        let mut store = state::Store::load_at(&home, &repo, None).unwrap();
        store.set_pull_request(None);
        store.save().unwrap();
        let resp = run(&repo, &home, add("a.txt", 3, None, "x"));
        assert!(!resp.ok && resp.error.unwrap().contains("recto pr"));
        let _ = std::fs::remove_dir_all(repo.parent().unwrap());
    }

    // The page shows each draft under the last line it's about.
    #[test]
    fn a_draft_renders_under_its_last_line() {
        let files = parse_diff(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,4 +1,4 @@\n 1\n-2\n+two\n 3\n 4\n",
        );
        let draft = DraftReviewComment {
            pin: Default::default(),
            id: 4,
            path: "a.txt".into(),
            start: 2,
            end: 3,
            body: "Say <why>".into(),
            last_editor: DraftEditor::Agent,
            revision: 1,
            snippet: None,
        };
        let mut out = String::new();
        render_files(
            0,
            &files,
            std::slice::from_ref(&draft),
            None,
            &mut 0,
            &mut out,
        );
        let card = out.find("data-draft=\"4\"").expect("rendered");
        let line3 = out.find(">3</td>").expect("line 3");
        assert!(line3 < card && card < out.find(">4</td>").unwrap(), "{out}");
        assert!(
            out.contains("Say &lt;why&gt;"),
            "raw html shows as text: {out}"
        );
    }

    // A draft goes by its ✎ number in the window, the same one the Review
    // tab gives it, not by its id: ids are creation order, the slate isn't.
    #[test]
    fn drafts_are_numbered_in_slate_order() {
        let files = parse_diff(
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,4 +1,4 @@\n 1\n-2\n+two\n 3\n 4\n",
        );
        let draft = |id, start, end| DraftReviewComment {
            pin: Default::default(),
            id,
            path: "a.txt".into(),
            start,
            end,
            body: format!("draft {id}"),
            last_editor: DraftEditor::Agent,
            revision: 1,
            snippet: None,
        };
        let mut drafts = vec![draft(1, 4, 4), draft(2, 1, 1), draft(3, 9, 9)];
        slate_order(&files, &mut drafts);
        assert_eq!(
            drafts.iter().map(|d| d.id).collect::<Vec<_>>(),
            [2, 1, 3],
            "by line, with the one off the diff last"
        );
        // Partly on the diff still counts as on it, as in the viewer's sort.
        let mut straddling = vec![draft(5, 3, 3), draft(4, 2, 12)];
        slate_order(&files, &mut straddling);
        assert_eq!(straddling.iter().map(|d| d.id).collect::<Vec<_>>(), [4, 5]);
        let mut out = String::new();
        render_files(0, &files, &drafts, None, &mut 0, &mut out);
        assert!(out.contains("data-draft=\"2\" data-n=1"), "{out}");
        assert!(out.contains("data-draft=\"1\" data-n=2"), "{out}");
        assert!(out.contains("✎2 <span class=dim>a.txt:4 · id 1"), "{out}");
    }
}
