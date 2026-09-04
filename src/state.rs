//! Durable authored state for every Recto workspace.
//!
//! Recto owns one atomic JSON document beneath the XDG state directory, keyed
//! by the canonical workspace root. Rig and standalone launches therefore use
//! the same persistence model. A lifecycle owner can ask Recto to forget a
//! workspace through the public CLI without knowing this layout or format.

use std::fs::{self, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::app::{AgentNote, Annotation, FocusAnchor, NoteDraft};
use crate::link::{DraftReviewBody, DraftReviewComment, PullRequest, PullRequestRef};

const SCHEMA_VERSION: u32 = 2;
const LEGACY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Deserialize, Serialize)]
struct NoteState {
    #[serde(default = "first_id")]
    next_id: u64,
    #[serde(default)]
    items: Vec<AgentNote>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    composer: Option<NoteDraft>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ReviewState {
    pull_request: PullRequestRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    body: Option<DraftReviewBody>,
    #[serde(default)]
    comments: Vec<DraftReviewComment>,
    #[serde(default = "first_id")]
    next_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    composer: Option<NoteDraft>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Document {
    schema_version: u32,
    workspace_root: PathBuf,
    #[serde(default)]
    notes: NoteState,
    #[serde(default)]
    reviews: Vec<ReviewState>,
    /// The literate tour, when one is laid down. Absent in documents written
    /// before tours existed, which reads as no tour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tour: Option<String>,
    /// Companion tour annotations, in step order.
    #[serde(default)]
    annotations: Vec<Annotation>,
    /// The active companion focus span, minus its arrival instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    focus: Option<FocusAnchor>,
    /// Whether published threads, shared drafts and private notes are woven
    /// into the diff and file tree. Documents written before this was durable
    /// omit it, which reads as shown.
    #[serde(default = "shown")]
    comments_visible: bool,
    /// The attached pull request snapshot. Fetched context rather than
    /// authored words, kept here so restoring it needs no network: the review
    /// drafts below are keyed to it and are unreachable without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pull_request: Option<PullRequest>,
}

#[derive(Debug, Deserialize, Serialize)]
struct LegacyDocument {
    schema_version: u32,
    repo: String,
    #[serde(default)]
    notes: NoteState,
    #[serde(default)]
    reviews: Vec<ReviewState>,
}

impl Document {
    fn empty(workspace_root: PathBuf) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            workspace_root,
            notes: NoteState {
                next_id: first_id(),
                ..NoteState::default()
            },
            reviews: Vec::new(),
            tour: None,
            annotations: Vec::new(),
            focus: None,
            comments_visible: true,
            pull_request: None,
        }
    }
}

pub struct Store {
    path: PathBuf,
    document: Document,
}

pub struct RestoredReview {
    pub body: Option<DraftReviewBody>,
    pub comments: Vec<DraftReviewComment>,
    pub next_id: u64,
    pub composer: Option<NoteDraft>,
}

impl Store {
    pub fn load(workspace_root: &Path, legacy_rig: Option<(&Path, &str)>) -> Result<Self> {
        Self::load_at(&state_home()?, workspace_root, legacy_rig)
    }

    pub(crate) fn load_at(
        state_home: &Path,
        workspace_root: &Path,
        legacy_rig: Option<(&Path, &str)>,
    ) -> Result<Self> {
        let workspace_root = normalize_workspace_root(workspace_root)?;
        let path = state_path_at(state_home, &workspace_root);
        let document = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => serde_json::from_reader::<_, Document>(BufReader::new(file))
                .with_context(|| format!("could not parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match legacy_rig {
                Some((rig_root, repo)) => load_legacy(rig_root, repo, &workspace_root)?
                    .unwrap_or_else(|| Document::empty(workspace_root.clone())),
                None => Document::empty(workspace_root.clone()),
            },
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", path.display()));
            }
        };
        if document.schema_version != SCHEMA_VERSION {
            return Err(anyhow!(
                "{} uses unsupported state schema {}",
                path.display(),
                document.schema_version
            ));
        }
        if document.workspace_root != workspace_root {
            return Err(anyhow!(
                "{} belongs to workspace {}",
                path.display(),
                document.workspace_root.display()
            ));
        }

        let store = Self { path, document };
        if let Some((rig_root, repo)) = legacy_rig {
            let legacy = legacy_path(rig_root, repo)?;
            if legacy.exists() {
                store.save()?;
                let _ = fs::remove_file(&legacy);
                if let Some(parent) = legacy.parent() {
                    let _ = fs::remove_dir(parent);
                }
            }
        }
        Ok(store)
    }

    pub fn notes(&self) -> (&[AgentNote], u64, Option<&NoteDraft>) {
        let after_items = self
            .document
            .notes
            .items
            .iter()
            .map(|note| note.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        (
            &self.document.notes.items,
            self.document.notes.next_id.max(after_items).max(1),
            self.document.notes.composer.as_ref(),
        )
    }

    pub fn set_notes(&mut self, items: &[AgentNote], next_id: u64) {
        self.document.notes.next_id = next_id.max(1);
        self.document.notes.items = items.to_vec();
    }

    pub fn set_note_composer(&mut self, composer: Option<&NoteDraft>) {
        self.document.notes.composer = composer.cloned();
    }

    pub fn tour(&self) -> Option<&str> {
        self.document.tour.as_deref()
    }

    pub fn set_tour(&mut self, tour: Option<&str>) {
        self.document.tour = tour.map(str::to_string);
    }

    pub fn annotations(&self) -> &[Annotation] {
        &self.document.annotations
    }

    pub fn set_annotations(&mut self, annotations: &[Annotation]) {
        self.document.annotations = annotations.to_vec();
    }

    pub fn focus(&self) -> Option<&FocusAnchor> {
        self.document.focus.as_ref()
    }

    pub fn set_focus(&mut self, focus: Option<&FocusAnchor>) {
        self.document.focus = focus.cloned();
    }

    pub fn comments_visible(&self) -> bool {
        self.document.comments_visible
    }

    pub fn set_comments_visible(&mut self, visible: bool) {
        self.document.comments_visible = visible;
    }

    pub fn pull_request(&self) -> Option<&PullRequest> {
        self.document.pull_request.as_ref()
    }

    pub fn set_pull_request(&mut self, pull_request: Option<&PullRequest>) {
        self.document.pull_request = pull_request.cloned();
    }

    pub fn review(&self, pull_request: &PullRequestRef) -> Option<RestoredReview> {
        self.document
            .reviews
            .iter()
            .find(|review| review.pull_request == *pull_request)
            .map(|review| RestoredReview {
                body: review.body.clone(),
                comments: review.comments.clone(),
                next_id: review
                    .next_id
                    .max(
                        review
                            .comments
                            .iter()
                            .map(|comment| comment.id)
                            .max()
                            .unwrap_or(0)
                            .saturating_add(1),
                    )
                    .max(1),
                composer: review.composer.clone(),
            })
    }

    pub fn set_review(
        &mut self,
        pull_request: PullRequestRef,
        body: Option<&DraftReviewBody>,
        comments: &[DraftReviewComment],
        next_id: u64,
    ) {
        let composer = self
            .document
            .reviews
            .iter()
            .find(|saved| saved.pull_request == pull_request)
            .and_then(|saved| saved.composer.clone());
        let review = ReviewState {
            pull_request: pull_request.clone(),
            body: body.cloned(),
            comments: comments.to_vec(),
            next_id: next_id.max(1),
            composer,
        };
        match self
            .document
            .reviews
            .iter()
            .position(|saved| saved.pull_request == pull_request)
        {
            Some(index) => self.document.reviews[index] = review,
            None => self.document.reviews.push(review),
        }
    }

    pub fn set_review_composer(
        &mut self,
        pull_request: &PullRequestRef,
        composer: Option<&NoteDraft>,
    ) {
        if let Some(review) = self
            .document
            .reviews
            .iter_mut()
            .find(|saved| saved.pull_request == *pull_request)
        {
            review.composer = composer.cloned();
        }
    }

    pub fn save(&self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("state path has no parent: {}", self.path.display()))?;
        protect_state_dirs(parent)?;

        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&temporary)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &self.document)
            .with_context(|| format!("could not encode {}", temporary.display()))?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("could not replace {}", self.path.display()))?;
        Ok(())
    }
}

pub fn forget(workspace_root: &Path) -> Result<()> {
    forget_at(&state_home()?, workspace_root)
}

fn forget_at(state_home: &Path, workspace_root: &Path) -> Result<()> {
    let workspace_root = normalize_workspace_root(workspace_root)?;
    let path = state_path_at(state_home, &workspace_root);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not remove {}", path.display()));
        }
    }
    if let Some(workspaces) = path.parent() {
        let _ = fs::remove_dir(workspaces);
        if let Some(recto) = workspaces.parent() {
            let _ = fs::remove_dir(recto);
        }
    }
    Ok(())
}

fn state_home() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Ok(path);
        }
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        anyhow!("cannot locate Recto state: neither XDG_STATE_HOME nor HOME is set")
    })?;
    Ok(PathBuf::from(home).join(".local/state"))
}

fn state_path_at(state_home: &Path, workspace_root: &Path) -> PathBuf {
    let digest = Sha256::digest(workspace_root.to_string_lossy().as_bytes());
    state_home
        .join("recto/workspaces")
        .join(format!("{digest:x}.json"))
}

fn normalize_workspace_root(workspace_root: &Path) -> Result<PathBuf> {
    match fs::canonicalize(workspace_root) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let absolute = if workspace_root.is_absolute() {
                workspace_root.to_path_buf()
            } else {
                std::env::current_dir()?.join(workspace_root)
            };
            Ok(clean_path(&absolute))
        }
        Err(error) => {
            Err(error).with_context(|| format!("could not resolve {}", workspace_root.display()))
        }
    }
}

fn clean_path(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            component => clean.push(component.as_os_str()),
        }
    }
    clean
}

fn legacy_path(rig_root: &Path, repo: &str) -> Result<PathBuf> {
    if Path::new(repo).components().count() != 1
        || !matches!(
            Path::new(repo).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err(anyhow!(
            "rig info returned invalid repository directory {repo:?}"
        ));
    }
    Ok(rig_root.join(".recto").join(format!("{repo}.json")))
}

fn load_legacy(rig_root: &Path, repo: &str, workspace_root: &Path) -> Result<Option<Document>> {
    let path = legacy_path(rig_root, repo)?;
    let file = match OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    let legacy = serde_json::from_reader::<_, LegacyDocument>(BufReader::new(file))
        .with_context(|| format!("could not parse {}", path.display()))?;
    if legacy.schema_version != LEGACY_SCHEMA_VERSION {
        return Err(anyhow!(
            "{} uses unsupported legacy state schema {}",
            path.display(),
            legacy.schema_version
        ));
    }
    if legacy.repo != repo {
        return Err(anyhow!(
            "{} belongs to repository directory {:?}",
            path.display(),
            legacy.repo
        ));
    }
    Ok(Some(Document {
        schema_version: SCHEMA_VERSION,
        workspace_root: workspace_root.to_path_buf(),
        notes: legacy.notes,
        reviews: legacy.reviews,
        tour: None,
        annotations: Vec::new(),
        focus: None,
        comments_visible: true,
        pull_request: None,
    }))
}

fn protect_state_dirs(workspaces: &Path) -> Result<()> {
    fs::create_dir_all(workspaces)
        .with_context(|| format!("could not create {}", workspaces.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for directory in [workspaces.parent(), Some(workspaces)]
            .into_iter()
            .flatten()
        {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("could not protect {}", directory.display()))?;
        }
    }
    Ok(())
}

fn first_id() -> u64 {
    1
}

const fn shown() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ComposerEdit, ComposerKind};

    fn roots(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "recto-state-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        let state_home = root.join("state");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        (state_home, workspace)
    }

    #[test]
    fn round_trips_notes_composers_and_pr_head_drafts() {
        let (state_home, workspace) = roots("round-trip");
        let mut store = Store::load_at(&state_home, &workspace, None).unwrap();
        let note = AgentNote {
            id: 4,
            path: "src/lib.rs".into(),
            start: 8,
            end: 9,
            body: "Keep this.".into(),
        };
        let note_composer = NoteDraft {
            kind: ComposerKind::AgentNote,
            anchor: Some(("src/main.rs".into(), 12)),
            body: "Half written".into(),
            caret: 5,
            error: None,
            editing: None,
        };
        store.set_notes(std::slice::from_ref(&note), 5);
        store.set_note_composer(Some(&note_composer));

        let pull_request = PullRequestRef {
            repository: "phinze/recto".into(),
            number: 8,
            head_oid: "abc123".into(),
        };
        let review_composer = NoteDraft {
            kind: ComposerKind::ReviewComment,
            anchor: Some(("src/state.rs".into(), 20)),
            body: "Also half written".into(),
            caret: 17,
            error: None,
            editing: Some(ComposerEdit::ReviewComment(7)),
        };
        store.set_review(
            pull_request.clone(),
            Some(&DraftReviewBody {
                body: "Overall".into(),
                last_editor: crate::link::DraftEditor::User,
            }),
            &[DraftReviewComment {
                id: 7,
                path: "src/state.rs".into(),
                start: 20,
                end: 21,
                body: "Inline".into(),
                last_editor: crate::link::DraftEditor::Agent,
            }],
            8,
        );
        store.set_review_composer(&pull_request, Some(&review_composer));
        store.save().unwrap();

        let restored = Store::load_at(&state_home, &workspace, None).unwrap();
        assert_eq!(restored.notes(), (&[note][..], 5, Some(&note_composer)));
        let review = restored.review(&pull_request).unwrap();
        assert_eq!(review.body.unwrap().body, "Overall");
        assert_eq!(review.comments[0].body, "Inline");
        assert_eq!(review.next_id, 8);
        assert_eq!(review.composer, Some(review_composer));
        assert!(!workspace.join(".recto").exists());
    }

    /// Durability is the rule now: everything a companion or reviewer put
    /// there comes back, and only an explicit discard takes it away.
    #[test]
    fn companion_state_round_trips_whole() {
        let (state_home, workspace) = roots("companion");
        let mut store = Store::load_at(&state_home, &workspace, None).unwrap();
        let annotation = Annotation {
            path: "src/link.rs".into(),
            start: 30,
            end: 34,
            label: "Step 2: the new request variant".into(),
        };
        let focus = FocusAnchor {
            path: "src/main.rs".into(),
            start: 12,
            end: 18,
        };
        store.set_annotations(std::slice::from_ref(&annotation));
        store.set_focus(Some(&focus));
        store.set_comments_visible(false);
        store.save().unwrap();

        let restored = Store::load_at(&state_home, &workspace, None).unwrap();
        assert_eq!(restored.annotations(), [annotation]);
        assert_eq!(restored.focus(), Some(&focus));
        assert!(!restored.comments_visible());
    }

    /// A document written before any of this was durable still loads, and
    /// reads as "comments shown" rather than as hidden.
    #[test]
    fn a_document_without_companion_state_reads_as_shown() {
        let (state_home, workspace) = roots("defaults");
        let store = Store::load_at(&state_home, &workspace, None).unwrap();
        assert!(store.comments_visible());
        assert!(store.annotations().is_empty());
        assert_eq!(store.focus(), None);
    }

    #[test]
    fn a_tour_round_trips_and_clears() {
        let (state_home, workspace) = roots("tour");
        let mut store = Store::load_at(&state_home, &workspace, None).unwrap();
        store.set_tour(Some("## Why\n\nBecause."));
        store.save().unwrap();

        let mut restored = Store::load_at(&state_home, &workspace, None).unwrap();
        assert_eq!(restored.tour(), Some("## Why\n\nBecause."));

        restored.set_tour(None);
        restored.save().unwrap();
        assert_eq!(
            Store::load_at(&state_home, &workspace, None)
                .unwrap()
                .tour(),
            None
        );
    }

    #[test]
    fn canonical_workspace_roots_get_distinct_external_documents() {
        let (state_home, workspace) = roots("keys");
        let other = workspace.parent().unwrap().join("other");
        fs::create_dir_all(&other).unwrap();
        let first = Store::load_at(&state_home, &workspace, None).unwrap();
        let second = Store::load_at(&state_home, &other, None).unwrap();
        assert_ne!(first.path, second.path);
        assert!(first.path.starts_with(state_home.join("recto/workspaces")));
    }

    #[test]
    fn migrates_the_colocated_rig_document_once() {
        let (state_home, workspace) = roots("migration");
        let rig_root = workspace.parent().unwrap();
        let legacy = legacy_path(rig_root, "workspace").unwrap();
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let document = LegacyDocument {
            schema_version: LEGACY_SCHEMA_VERSION,
            repo: "workspace".into(),
            notes: NoteState {
                next_id: 2,
                items: vec![AgentNote {
                    id: 1,
                    path: "src/main.rs".into(),
                    start: 4,
                    end: 4,
                    body: "Do not lose me".into(),
                }],
                composer: None,
            },
            reviews: Vec::new(),
        };
        fs::write(&legacy, serde_json::to_vec(&document).unwrap()).unwrap();

        let store = Store::load_at(&state_home, &workspace, Some((rig_root, "workspace"))).unwrap();
        assert_eq!(store.notes().0[0].body, "Do not lose me");
        assert!(store.path.exists());
        assert!(!legacy.exists());
    }

    #[test]
    fn forget_is_idempotent_and_accepts_a_removed_workspace() {
        let (state_home, workspace) = roots("forget");
        let store = Store::load_at(&state_home, &workspace, None).unwrap();
        store.save().unwrap();
        let path = store.path.clone();
        fs::remove_dir_all(&workspace).unwrap();

        forget_at(&state_home, &workspace).unwrap();
        forget_at(&state_home, &workspace).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn rejects_a_repository_path_in_legacy_context() {
        let (state_home, workspace) = roots("legacy-path");
        assert!(
            Store::load_at(
                &state_home,
                &workspace,
                Some((workspace.parent().unwrap(), "../other"))
            )
            .is_err()
        );
    }

    #[test]
    fn a_new_pr_head_does_not_inherit_the_old_heads_draft() {
        let (state_home, workspace) = roots("heads");
        let mut store = Store::load_at(&state_home, &workspace, None).unwrap();
        let old = PullRequestRef {
            repository: "phinze/recto".into(),
            number: 8,
            head_oid: "old-head".into(),
        };
        let new = PullRequestRef {
            head_oid: "new-head".into(),
            ..old.clone()
        };
        store.set_review(
            old.clone(),
            Some(&DraftReviewBody {
                body: "Only for the old diff".into(),
                last_editor: crate::link::DraftEditor::User,
            }),
            &[],
            1,
        );

        assert!(store.review(&old).is_some());
        assert!(store.review(&new).is_none());
    }
}
