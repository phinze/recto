//! Fetch and normalize the public GitHub context attached to a Recto session.

use std::process::Command;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use crate::link;

#[derive(Debug, PartialEq, Eq)]
struct PrLocator {
    repository: String,
    number: u64,
}

fn parse_pr_locator(raw: &str) -> Result<PrLocator> {
    let raw = raw.trim().trim_end_matches('/');
    let path = raw
        .strip_prefix("https://github.com/")
        .or_else(|| raw.strip_prefix("http://github.com/"))
        .unwrap_or(raw);
    if let Some((repository, number)) = path.split_once('#') {
        validate_repository(repository)?;
        return Ok(PrLocator {
            repository: repository.to_string(),
            number: number
                .parse()
                .map_err(|_| anyhow!("invalid PR number in `{raw}`"))?,
        });
    }
    let parts: Vec<&str> = path.split('/').collect();
    if let [owner, repo, "pull", number] = parts.as_slice() {
        let repository = format!("{owner}/{repo}");
        validate_repository(&repository)?;
        return Ok(PrLocator {
            repository,
            number: number
                .parse()
                .map_err(|_| anyhow!("invalid PR number in `{raw}`"))?,
        });
    }
    Err(anyhow!(
        "PR locator must be a GitHub pull URL or OWNER/REPO#NUMBER: `{raw}`"
    ))
}

fn validate_repository(repository: &str) -> Result<()> {
    let mut parts = repository.split('/');
    if parts.next().is_some_and(|s| !s.is_empty())
        && parts.next().is_some_and(|s| !s.is_empty())
        && parts.next().is_none()
    {
        Ok(())
    } else {
        Err(anyhow!("invalid GitHub repository `{repository}`"))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: u64,
    title: String,
    body: String,
    author: Option<GhActor>,
    base_ref_name: String,
    head_ref_name: String,
    head_ref_oid: String,
    url: String,
    comments: Vec<GhConversationComment>,
    reviews: Vec<GhReviewSummary>,
}

#[derive(Deserialize)]
struct GhActor {
    login: String,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhConversationComment {
    author: Option<GhActor>,
    body: String,
    created_at: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReviewSummary {
    author: Option<GhActor>,
    body: String,
    state: String,
    #[serde(default)]
    submitted_at: Option<String>,
    #[serde(default)]
    commit: Option<GhCommit>,
}

#[derive(Deserialize)]
struct GhCommit {
    oid: String,
}

#[derive(Deserialize)]
struct GhGraphQlResponse {
    data: GhGraphQlData,
}

#[derive(Deserialize)]
struct GhGraphQlData {
    repository: GhGraphQlRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhGraphQlRepository {
    pull_request: GhGraphQlPullRequest,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhGraphQlPullRequest {
    review_threads: GhReviewThreadConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReviewThreadConnection {
    page_info: GhPageInfo,
    nodes: Vec<GhReviewThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReviewThread {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    path: String,
    line: Option<u32>,
    original_line: Option<u32>,
    start_line: Option<u32>,
    original_start_line: Option<u32>,
    diff_side: String,
    comments: GhReviewCommentConnection,
}

#[derive(Deserialize)]
struct GhReviewCommentConnection {
    nodes: Vec<GhReviewComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReviewComment {
    id: String,
    database_id: Option<u64>,
    author: Option<GhActor>,
    body: String,
    created_at: String,
    url: String,
    reply_to: Option<GhNodeRef>,
}

#[derive(Deserialize)]
struct GhNodeRef {
    id: String,
}

const REVIEW_THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $endCursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $endCursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path line originalLine startLine originalStartLine diffSide
          comments(first: 100) {
            nodes { id databaseId author { login } body createdAt url replyTo { id } }
          }
        }
      }
    }
  }
}
"#;

pub(crate) fn fetch_pull_request(raw: &str) -> Result<link::PullRequest> {
    let locator = parse_pr_locator(raw)?;
    let number = locator.number.to_string();
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            &number,
            "-R",
            &locator.repository,
            "--json",
            "number,title,body,author,baseRefName,headRefName,headRefOid,url,comments,reviews",
        ])
        .output()
        .map_err(|e| anyhow!("could not run gh: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow!(
            "could not fetch {}/#{}: {}",
            locator.repository,
            locator.number,
            if stderr.is_empty() {
                "gh failed without an error message"
            } else {
                &stderr
            }
        ));
    }
    let gh: GhPullRequest = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow!("could not decode gh PR response: {e}"))?;
    let threads = fetch_review_threads(&locator)?;
    Ok(link::PullRequest {
        repository: locator.repository,
        number: gh.number,
        title: gh.title,
        body: normalize_github_text(gh.body),
        author: actor_or_ghost(gh.author),
        base_ref: gh.base_ref_name,
        head_ref: gh.head_ref_name,
        head_oid: gh.head_ref_oid,
        url: gh.url,
        conversation: gh
            .comments
            .into_iter()
            .map(|comment| link::ConversationComment {
                author: actor_or_ghost(comment.author),
                body: normalize_github_text(comment.body),
                created_at: comment.created_at,
                url: comment.url,
            })
            .collect(),
        reviews: gh
            .reviews
            .into_iter()
            .map(|review| link::ReviewSummary {
                author: actor_or_ghost(review.author),
                body: normalize_github_text(review.body),
                state: parse_review_state(&review.state),
                submitted_at: review.submitted_at,
                commit_oid: review.commit.map(|commit| commit.oid),
            })
            .collect(),
        threads,
    })
}

fn fetch_review_threads(locator: &PrLocator) -> Result<Vec<link::ReviewThread>> {
    let (owner, name) = locator
        .repository
        .split_once('/')
        .expect("validated owner/repository");
    let mut after = None;
    let mut threads = Vec::new();
    loop {
        let mut command = Command::new("gh");
        command
            .args([
                "api",
                "graphql",
                "-f",
                &format!("query={REVIEW_THREADS_QUERY}"),
            ])
            .args(["-f", &format!("owner={owner}")])
            .args(["-f", &format!("name={name}")])
            .args(["-F", &format!("number={}", locator.number)]);
        if let Some(cursor) = &after {
            command.args(["-f", &format!("endCursor={cursor}")]);
        }
        let output = command
            .output()
            .map_err(|e| anyhow!("could not run gh: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(anyhow!(
                "could not fetch review threads for {}#{}: {}",
                locator.repository,
                locator.number,
                if stderr.is_empty() {
                    "gh failed without an error message"
                } else {
                    &stderr
                }
            ));
        }
        let page: GhGraphQlResponse = serde_json::from_slice(&output.stdout)
            .map_err(|e| anyhow!("could not decode gh review-thread response: {e}"))?;
        let connection = page.data.repository.pull_request.review_threads;
        threads.extend(connection.nodes.into_iter().map(normalize_review_thread));
        if !connection.page_info.has_next_page {
            break;
        }
        after = connection.page_info.end_cursor;
        if after.is_none() {
            return Err(anyhow!(
                "GitHub reported another thread page without a cursor"
            ));
        }
    }
    Ok(threads)
}

fn normalize_review_thread(thread: GhReviewThread) -> link::ReviewThread {
    link::ReviewThread {
        id: thread.id,
        path: thread.path,
        side: match thread.diff_side.as_str() {
            "LEFT" => link::DiffSide::Left,
            "RIGHT" => link::DiffSide::Right,
            _ => link::DiffSide::Unknown,
        },
        line: thread.line,
        start_line: thread.start_line,
        original_line: thread.original_line,
        original_start_line: thread.original_start_line,
        resolved: thread.is_resolved,
        outdated: thread.is_outdated,
        comments: thread
            .comments
            .nodes
            .into_iter()
            .map(|comment| link::ReviewComment {
                id: comment.id,
                database_id: comment.database_id,
                author: actor_or_ghost(comment.author),
                body: normalize_github_text(comment.body),
                created_at: comment.created_at,
                url: comment.url,
                reply_to: comment.reply_to.map(|reply| reply.id),
            })
            .collect(),
    }
}

impl From<GhActor> for link::Actor {
    fn from(actor: GhActor) -> Self {
        Self {
            login: actor.login,
            name: actor.name,
        }
    }
}

fn actor_or_ghost(actor: Option<GhActor>) -> link::Actor {
    actor.map(Into::into).unwrap_or_else(|| link::Actor {
        login: "ghost".into(),
        name: None,
    })
}

fn normalize_github_text(text: String) -> String {
    text.replace("\r\n", "\n")
}

fn parse_review_state(state: &str) -> link::ReviewState {
    match state {
        "APPROVED" => link::ReviewState::Approved,
        "CHANGES_REQUESTED" => link::ReviewState::ChangesRequested,
        "COMMENTED" => link::ReviewState::Commented,
        "DISMISSED" => link::ReviewState::Dismissed,
        "PENDING" => link::ReviewState::Pending,
        _ => link::ReviewState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_locator_accepts_url_and_compact_forms() {
        assert_eq!(
            parse_pr_locator("https://github.com/cli/cli/pull/14136/").unwrap(),
            PrLocator {
                repository: "cli/cli".into(),
                number: 14136,
            }
        );
        assert_eq!(
            parse_pr_locator("phinze/recto#7").unwrap(),
            PrLocator {
                repository: "phinze/recto".into(),
                number: 7,
            }
        );
        assert!(parse_pr_locator("14136").is_err());
    }
}
