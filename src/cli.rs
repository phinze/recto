//! The client half of the binary: the subcommands that drive an already
//! running recto over its workspace socket, rather than opening a TUI.
//!
//! Nothing here touches `App`. A client command is parsed, turned into a
//! `link::Request`, written to the socket, and answered with a process exit
//! code — which is why this half can sit beside the viewer instead of inside
//! it.

use std::io;
use std::path::Path;

use anyhow::{Result, anyhow};
use clap::{Subcommand, ValueEnum};

use crate::highlight::ext_for_path;
use crate::{github, link, parse_pathspec, state};

/// Subcommands that talk to an already-running recto over its workspace socket.
#[derive(Subcommand, Debug)]
pub(crate) enum ClientCommand {
    /// Inspect or remove Recto's durable state without opening the TUI.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
    /// Focus a file or span in the running recto. PATHSPEC is `path`,
    /// `path:LINE`, or `path:START-END` (new-side line numbers).
    Focus { pathspec: String },
    /// Annotate spans in the running recto with numbered labels. Each SPEC is
    /// `path:LINE=label` or `path:START-END=label`; argument order sets the
    /// step numbers, and the new set replaces any previous one.
    Annotate {
        #[arg(required = true)]
        specs: Vec<String>,
    },
    /// Lay down a literate tour: a Markdown document whose headings become
    /// navigable sections and whose fenced `recto PATH:SPAN` blocks quote the
    /// diff inline. Reads BODY, or stdin when BODY is omitted. An empty body
    /// removes the tour. Coexists with `annotate`.
    Tour { body: Option<String> },
    /// Bring the tour into view, optionally scrolled to a numbered SECTION.
    /// Sections are numbered from 1, matching the outline rail's badges.
    TourFocus { section: Option<usize> },
    /// Clear any active focus highlight and annotations in the running recto.
    Clear,
    /// Control visibility of published threads, shared drafts, and private
    /// notes without affecting tour annotations.
    CommentVisibility {
        #[arg(value_enum, default_value_t = VisibilityAction::Toggle)]
        action: VisibilityAction,
    },
    /// Check that a recto is listening for this workspace.
    Ping,
    /// Fetch and open a GitHub PR in the running recto, selecting its exact
    /// base. LOCATOR is a full PR URL or `OWNER/REPO#NUMBER`.
    Pr { locator: String },
    /// Leave a private note for the local agent. SPEC is
    /// `path:LINE=body` or `path:START-END=body`. Notes accumulate; run
    /// this once per note.
    #[command(alias = "comment")]
    Note { spec: String },
    /// Read pending agent notes as agent-ready markdown. After acting, pass
    /// their stable ids with --ack to remove only those notes.
    #[command(alias = "comments")]
    Notes {
        #[arg(long, num_args = 1..)]
        ack: Vec<u64>,
    },
    /// Show the local review draft as JSON. This is a non-consuming read and
    /// may be called throughout co-authoring.
    Review,
    /// Add, revise, or delete the shared top-level review body. An empty BODY
    /// deletes the draft.
    ReviewBody { body: String },
    /// Add or revise a shared public review comment. Without --id, INPUT is
    /// `path:LINE=body` or `path:START-END=body`. With --id, INPUT is the new
    /// body; an empty body deletes that draft.
    ReviewComment {
        #[arg(long)]
        id: Option<u64>,
        input: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum VisibilityAction {
    Show,
    Hide,
    Toggle,
}

#[derive(Subcommand, Debug)]
pub(crate) enum StateCommand {
    /// Remove the authored state associated with a workspace root.
    Forget {
        #[arg(long, value_name = "PATH")]
        workspace_root: std::path::PathBuf,
    },
}

/// Run a client subcommand against the workspace's running recto. Returns the
/// process exit code: 0 on `{"ok":true}`, 1 on a refused request (e.g. target
/// not in the diff), 2 when we couldn't reach a recto at all.
pub(crate) fn run_client(command: ClientCommand) -> i32 {
    if let ClientCommand::State { command } = &command {
        return run_state_command(command);
    }
    let socket = match link::socket_for_cwd() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("recto: {e}");
            return 2;
        }
    };
    let request = match build_request(&command) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("recto: {e}");
            return 2;
        }
    };
    match link::send(&socket, &request) {
        Ok(resp) if resp.ok => {
            // Status (from `ping`) is the machine-readable payload: emit it as
            // JSON on stdout, keeping human notes on stderr so a script can read
            // one without the other.
            if let Some(status) = &resp.status {
                match serde_json::to_string_pretty(status) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("recto: could not encode status: {e}");
                        return 2;
                    }
                }
            }
            // A note read's payload is markdown on stdout, so it can be piped
            // straight into a prompt. An empty read writes nothing there —
            // "no comments" belongs on stderr with the other asides.
            if let Some(comments) = &resp.comments {
                if comments.is_empty() {
                    eprintln!("recto: no agent notes pending");
                } else {
                    print!("{}", render_agent_notes_markdown(comments));
                }
            }
            if let Some(draft) = &resp.review_draft {
                match serde_json::to_string_pretty(draft) {
                    Ok(json) => println!("{json}"),
                    Err(e) => {
                        eprintln!("recto: could not encode review draft: {e}");
                        return 2;
                    }
                }
            }
            if let Some(note) = resp.note {
                eprintln!("recto: {note}");
            }
            0
        }
        Ok(resp) => {
            eprintln!(
                "recto: {}",
                resp.error.unwrap_or_else(|| "request refused".into())
            );
            1
        }
        Err(e) => {
            eprintln!("recto: {e}");
            2
        }
    }
}

fn run_state_command(command: &StateCommand) -> i32 {
    let result = match command {
        StateCommand::Forget { workspace_root } => state::forget(workspace_root),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("recto: {error}");
            2
        }
    }
}

/// Turn a CLI subcommand into a wire [`link::Request`], normalizing focus paths
/// to workspace-root-relative so the agent can pass whatever form it used.
fn build_request(command: &ClientCommand) -> Result<link::Request> {
    match command {
        ClientCommand::State { .. } => {
            Err(anyhow!("state commands do not use the workspace socket"))
        }
        ClientCommand::Ping => Ok(link::Request::Ping),
        ClientCommand::Pr { locator } => Ok(link::Request::AttachPr {
            pull_request: Box::new(github::fetch_pull_request(locator)?),
        }),
        ClientCommand::Tour { body } => {
            use std::io::{IsTerminal, Read};
            let body = match body {
                Some(body) => body.clone(),
                None => {
                    // A tour document is far too long to be comfortable as an
                    // argument, so a pipe is the expected shape. Refuse rather
                    // than block forever when nothing is piped in.
                    let mut stdin = io::stdin();
                    if stdin.is_terminal() {
                        return Err(anyhow!(
                            "recto tour needs a Markdown body as an argument or on stdin"
                        ));
                    }
                    let mut buffer = String::new();
                    stdin.read_to_string(&mut buffer)?;
                    buffer
                }
            };
            Ok(link::Request::Tour { body })
        }
        ClientCommand::TourFocus { section } => Ok(link::Request::TourFocus { section: *section }),
        ClientCommand::Clear => Ok(link::Request::Clear),
        ClientCommand::CommentVisibility { action } => Ok(link::Request::CommentVisibility {
            visible: match action {
                VisibilityAction::Show => Some(true),
                VisibilityAction::Hide => Some(false),
                VisibilityAction::Toggle => None,
            },
        }),
        ClientCommand::Focus { pathspec } => {
            let (raw_path, start, end) = parse_pathspec(pathspec);
            let cwd = std::env::current_dir()?;
            let root = link::workspace_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a jj or git repository"))?;
            Ok(link::Request::Focus {
                path: normalize_path(&cwd, &root, raw_path),
                start,
                end,
            })
        }
        ClientCommand::Annotate { specs } => {
            let cwd = std::env::current_dir()?;
            let root = link::workspace_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a jj or git repository"))?;
            let sites = specs
                .iter()
                .map(|spec| {
                    let (pathspec, label) = spec
                        .split_once('=')
                        .ok_or_else(|| anyhow!("missing `=label` in annotate spec: {spec}"))?;
                    let (raw_path, start, end) = parse_pathspec(pathspec);
                    let start =
                        start.ok_or_else(|| anyhow!("missing `:LINE` in annotate spec: {spec}"))?;
                    Ok(link::Site {
                        path: normalize_path(&cwd, &root, raw_path),
                        start,
                        end,
                        label: label.to_string(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(link::Request::Annotate { sites })
        }
        ClientCommand::Notes { ack } if ack.is_empty() => Ok(link::Request::ReadAgentNotes),
        ClientCommand::Notes { ack } => {
            Ok(link::Request::AcknowledgeAgentNotes { ids: ack.clone() })
        }
        ClientCommand::Review => Ok(link::Request::ReviewDraft),
        ClientCommand::ReviewBody { body } => {
            Ok(link::Request::ReviewDraftBody { body: body.clone() })
        }
        ClientCommand::ReviewComment {
            id: Some(id),
            input,
        } => Ok(link::Request::ReviewDraftComment {
            id: Some(*id),
            path: None,
            start: None,
            end: None,
            body: input.clone(),
        }),
        ClientCommand::ReviewComment { id: None, input } => {
            let (pathspec, body) = input
                .split_once('=')
                .ok_or_else(|| anyhow!("missing `=body` in review-comment input: {input}"))?;
            let (raw_path, start, end) = parse_pathspec(pathspec);
            let start =
                start.ok_or_else(|| anyhow!("missing `:LINE` in review-comment input: {input}"))?;
            let cwd = std::env::current_dir()?;
            let root = link::workspace_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a jj or git repository"))?;
            Ok(link::Request::ReviewDraftComment {
                id: None,
                path: Some(normalize_path(&cwd, &root, raw_path)),
                start: Some(start),
                end,
                body: body.to_string(),
            })
        }
        ClientCommand::Note { spec } => {
            let (pathspec, body) = spec
                .split_once('=')
                .ok_or_else(|| anyhow!("missing `=body` in note spec: {spec}"))?;
            let (raw_path, start, end) = parse_pathspec(pathspec);
            let start = start.ok_or_else(|| anyhow!("missing `:LINE` in note spec: {spec}"))?;
            let cwd = std::env::current_dir()?;
            let root = link::workspace_root(&cwd)
                .ok_or_else(|| anyhow!("not inside a jj or git repository"))?;
            Ok(link::Request::AgentNote {
                path: normalize_path(&cwd, &root, raw_path),
                start,
                end,
                body: body.to_string(),
            })
        }
    }
}

/// Format a pending note set as the markdown an agent reads. Each note leads
/// with its number and `path:line`, then quotes the diff rows it points at, so
/// the agent can act without re-opening the file — and so the note still makes
/// sense after its own edits have moved those line numbers.
fn render_agent_notes_markdown(comments: &[link::AgentNote]) -> String {
    let mut out = format!("# Agent notes ({})\n\n", comments.len());
    out.push_str(
        "Private notes the user left for the local agent on the current diff. Reading does not \
         remove them. Line numbers are new-side; `>` \
         marks the lines a note points at.\n",
    );
    for c in comments {
        let span = if c.end > c.start {
            format!("{}-{}", c.start, c.end)
        } else {
            c.start.to_string()
        };
        out.push_str(&format!(
            "\n## {}. {}:{}\n\n{}\n",
            c.n, c.path, span, c.body
        ));
        let Some(rows) = &c.snippet else { continue };
        out.push_str(&format!("\n```{}\n", ext_for_path(&c.path)));
        for r in rows {
            let mark = if r.commented { '>' } else { ' ' };
            match r.line {
                Some(n) => out.push_str(&format!("{mark}{n:>5} {} {}\n", r.sign, r.text)),
                None => out.push_str(&format!("{mark}      {} {}\n", r.sign, r.text)),
            }
        }
        out.push_str("```\n");
    }
    let ids = comments
        .iter()
        .map(|comment| comment.id.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    out.push_str(&format!(
        "\nAfter acting on every note above, acknowledge exactly this set with:\n\n    recto notes --ack {ids}\n"
    ));
    out
}

/// Resolve `raw` (absolute or cwd-relative) to a path relative to the workspace
/// root, matching the form the backend reports in `FileChange::path`. Falls back
/// to the input unchanged if it can't be placed under the root.
fn normalize_path(cwd: &Path, root: &Path, raw: &str) -> String {
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let abs = abs.canonicalize().unwrap_or(abs);
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    abs.strip_prefix(&root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_body_command_preserves_markdown() {
        let request = build_request(&ClientCommand::ReviewBody {
            body: "## Summary\n\nLooks good.".into(),
        })
        .unwrap();

        assert!(matches!(
            request,
            link::Request::ReviewDraftBody { body }
                if body == "## Summary\n\nLooks good."
        ));
    }

    #[test]
    fn comment_visibility_command_maps_show_hide_and_toggle() {
        let request = |action| build_request(&ClientCommand::CommentVisibility { action }).unwrap();
        assert!(matches!(
            request(VisibilityAction::Show),
            link::Request::CommentVisibility {
                visible: Some(true)
            }
        ));
        assert!(matches!(
            request(VisibilityAction::Hide),
            link::Request::CommentVisibility {
                visible: Some(false)
            }
        ));
        assert!(matches!(
            request(VisibilityAction::Toggle),
            link::Request::CommentVisibility { visible: None }
        ));
    }

    #[test]
    fn notes_ack_command_carries_stable_ids() {
        assert!(matches!(
            build_request(&ClientCommand::Notes { ack: vec![] }).unwrap(),
            link::Request::ReadAgentNotes
        ));
        let request = build_request(&ClientCommand::Notes { ack: vec![4, 5] }).unwrap();
        assert!(matches!(
            request,
            link::Request::AcknowledgeAgentNotes { ids } if ids == [4, 5]
        ));
    }

    /// The note markdown is the actual agent-facing contract: number and
    /// location first so a note is addressable, then the body, then the quoted
    /// rows with `>` on the ones being commented on.
    #[test]
    fn comment_markdown_quotes_the_span() {
        let md = render_agent_notes_markdown(&[link::AgentNote {
            id: 7,
            n: 1,
            path: "src/main.rs".into(),
            start: 42,
            end: 42,
            body: "why 3?".into(),
            snippet: Some(vec![
                link::SnippetRow {
                    line: Some(41),
                    sign: ' ',
                    text: "let a = 1;".into(),
                    commented: false,
                },
                link::SnippetRow {
                    line: None,
                    sign: '-',
                    text: "let b = 2;".into(),
                    commented: true,
                },
                link::SnippetRow {
                    line: Some(42),
                    sign: '+',
                    text: "let b = 3;".into(),
                    commented: true,
                },
            ]),
        }]);
        assert!(md.starts_with("# Agent notes (1)\n"));
        assert!(md.contains("## 1. src/main.rs:42\n\nwhy 3?\n"));
        assert!(md.contains("```rs\n"));
        assert!(md.contains("    41   let a = 1;\n"));
        assert!(md.contains(">      - let b = 2;\n"));
        assert!(md.contains(">   42 + let b = 3;\n"));
        assert!(md.contains("recto notes --ack 7"));
    }

    /// A range renders as `start-end`, and a comment whose span fell out of the
    /// diff still has to arrive — just without its quote.
    #[test]
    fn comment_markdown_handles_ranges_and_missing_snippets() {
        let md = render_agent_notes_markdown(&[link::AgentNote {
            id: 8,
            n: 2,
            path: "src/link.rs".into(),
            start: 10,
            end: 14,
            body: "extract this".into(),
            snippet: None,
        }]);
        assert!(md.contains("## 2. src/link.rs:10-14\n\nextract this\n"));
        assert!(!md.contains("```"));
    }
}
