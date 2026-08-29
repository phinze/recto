//! Durable review state scoped to a Rig's lifetime.
//!
//! Rig tells Recto where the rig root is through its public JSON API. Recto
//! owns the `.recto/` directory and everything in this file; it never reads
//! Rig's manifest. Each repository gets one small atomic JSON document, so the
//! Rectos in a multi-repository rig never contend on a write.

use std::fs::{self, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::link::{DraftReviewBody, DraftReviewComment, PullRequestRef};
use crate::{AgentNote, NoteDraft};

const SCHEMA_VERSION: u32 = 1;

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
    repo: String,
    #[serde(default)]
    notes: NoteState,
    #[serde(default)]
    reviews: Vec<ReviewState>,
}

impl Document {
    fn empty(repo: String) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            repo,
            notes: NoteState {
                next_id: first_id(),
                ..NoteState::default()
            },
            reviews: Vec::new(),
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
    pub fn load(rig_root: &Path, repo: &str) -> Result<Self> {
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
        let path = rig_root.join(".recto").join(format!("{repo}.json"));
        let document = match OpenOptions::new().read(true).open(&path) {
            Ok(file) => serde_json::from_reader::<_, Document>(BufReader::new(file))
                .with_context(|| format!("could not parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Document::empty(repo.to_string())
            }
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
        if document.repo != repo {
            return Err(anyhow!(
                "{} belongs to repository directory {:?}",
                path.display(),
                document.repo
            ));
        }
        Ok(Self { path, document })
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
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("could not protect {}", parent.display()))?;
        }

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

fn first_id() -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ComposerEdit, ComposerKind};

    #[test]
    fn round_trips_notes_composers_and_pr_head_drafts() {
        let root =
            std::env::temp_dir().join(format!("recto-state-round-trip-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let _ = fs::remove_dir_all(root.join(".recto"));
        let mut store = Store::load(&root, "recto").unwrap();
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
        store.set_notes(&[note.clone()], 5);
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

        let restored = Store::load(&root, "recto").unwrap();
        assert_eq!(restored.notes(), (&[note][..], 5, Some(&note_composer)));
        let review = restored.review(&pull_request).unwrap();
        assert_eq!(review.body.unwrap().body, "Overall");
        assert_eq!(review.comments[0].body, "Inline");
        assert_eq!(review.next_id, 8);
        assert_eq!(review.composer, Some(review_composer));
    }

    #[test]
    fn rejects_a_repository_path_instead_of_escaping_recto_state() {
        let root = std::env::temp_dir();
        assert!(Store::load(&root, "../other").is_err());
        assert!(Store::load(&root, "nested/repo").is_err());
    }

    #[test]
    fn a_new_pr_head_does_not_inherit_the_old_heads_draft() {
        let root = std::env::temp_dir().join(format!("recto-state-heads-{}", std::process::id()));
        let _ = fs::remove_dir_all(root.join(".recto"));
        let mut store = Store::load(&root, "recto").unwrap();
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
