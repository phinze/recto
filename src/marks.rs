//! Identities ("marks") for the parts of an agent-written document a person
//! answers or comments on: each `ask`/`do` block and each H1/H2 heading.
//!
//! A rig's comment log lives as long as the rig, so an agent-chosen ask id or
//! heading title can't distinguish last week's briefing from today's rewrite.
//! Recto mints a mark for each part when the document is saved, keeps it while
//! the part's content stays the same, and passes it to the web window to
//! anchor answers and comments. Rewording a part, or dropping and re-adding it,
//! mints a new mark.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use sha2::{Digest, Sha256};

/// A block in a briefing that waits on the person: an `ask` is a question to
/// answer, and a `do` is an action to mark done. Both keep the briefing open
/// until settled and take answers the same way.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Need {
    Ask,
    Do,
}

/// Parses a code fence's info string as an `ask` or `do` block, returning its
/// kind and optional id.
pub fn need_fence(info: &str) -> Option<(Need, Option<String>)> {
    let info = info.trim();
    let (kind, rest) = if let Some(rest) = info.strip_prefix("ask") {
        (Need::Ask, rest)
    } else if let Some(rest) = info.strip_prefix("do") {
        (Need::Do, rest)
    } else {
        return None;
    };
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    Some((kind, rest.split_whitespace().next().map(str::to_string)))
}

/// Returns the explicit block id, or falls back to a 1-based positional name
/// like `ask1` or `do2`.
pub fn need_id(kind: Need, id: Option<String>, n: usize) -> String {
    id.unwrap_or_else(|| match kind {
        Need::Ask => format!("ask{n}"),
        Need::Do => format!("do{n}"),
    })
}

/// Shared Markdown parser for agent-written documents so every reader agrees
/// on block and heading boundaries.
pub fn parser(source: &str) -> Parser<'_> {
    Parser::new_ext(source, Options::all() - Options::ENABLE_SMART_PUNCTUATION)
}

/// One answerable or commentable part: its document-local key and a content
/// digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub key: String,
    pub digest: String,
}

/// Extracted parts of a document, each in document order.
#[derive(Debug, Default)]
pub struct Parts {
    pub asks: Vec<Part>,
    pub headings: Vec<Part>,
}

/// Marks assigned to each part, aligned with `Parts`. `None` means the
/// part was saved before marks existed, or by an older binary.
#[derive(Debug, Default)]
pub struct Ids {
    pub asks: Vec<Option<String>>,
    pub headings: Vec<Option<String>>,
}

impl Ids {
    pub fn ask(&self, n: usize) -> Option<&str> {
        self.asks.get(n).and_then(|m| m.as_deref())
    }

    pub fn heading(&self, n: usize) -> Option<&str> {
        self.headings.get(n).and_then(|m| m.as_deref())
    }
}

/// Extracts asks and H1/H2 sections from Markdown source.
///
/// An ask's digest covers its kind, id, and body text. A heading's key
/// disambiguates duplicate titles by occurrence count, and its digest covers
/// the section text up to the next H1/H2 because comments apply to what the
/// section says, not just its title.
pub fn parts(source: &str) -> Parts {
    let mut out = Parts::default();
    let mut asking: Option<(Need, String, String)> = None;
    let mut asks = 0;
    // Title and byte offset where each H1/H2 section starts.
    let mut heads: Vec<(String, usize)> = Vec::new();
    let mut title: Option<String> = None;
    for (event, range) in parser(source).into_offset_iter() {
        if let Some((_, _, text)) = asking.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    if let Some((kind, id, text)) = asking.take() {
                        let tag = if kind == Need::Ask { "ask" } else { "do" };
                        out.asks.push(Part {
                            digest: digest(&[tag, &id, &text]),
                            key: id,
                        });
                    }
                }
                Event::Text(t) => text.push_str(&t),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => {
                if let Some((kind, id)) = need_fence(&info) {
                    asks += 1;
                    asking = Some((kind, need_id(kind, id, asks), String::new()));
                }
            }
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1 | HeadingLevel::H2,
                ..
            }) => {
                title = Some(String::new());
                heads.push((String::new(), range.start));
            }
            Event::End(TagEnd::Heading(HeadingLevel::H1 | HeadingLevel::H2)) => {
                if let (Some(t), Some(last)) = (title.take(), heads.last_mut()) {
                    last.0 = t;
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(title) = title.as_mut() {
                    title.push_str(&t);
                }
            }
            _ => {}
        }
    }
    for (i, (title, start)) in heads.iter().enumerate() {
        let end = heads.get(i + 1).map_or(source.len(), |next| next.1);
        let seen = heads[..i].iter().filter(|(t, _)| t == title).count();
        let key = if seen == 0 {
            title.clone()
        } else {
            format!("{title}\n{seen}")
        };
        out.headings.push(Part {
            digest: digest(&[&key, source[*start..end].trim()]),
            key,
        });
    }
    out
}

fn digest(fields: &[&str]) -> String {
    let mut h = Sha256::new();
    for f in fields {
        h.update(f.as_bytes());
        h.update([0]);
    }
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Mints a fresh mark unique across the life of a rig, even if the same text
/// is dropped and added back later.
pub fn mint(part: &Part) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
        .to_string();
    let n = COUNT.fetch_add(1, Ordering::Relaxed).to_string();
    let pid = std::process::id().to_string();
    format!(
        "m{}",
        &digest(&[&part.key, &part.digest, &now, &n, &pid])[..12]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ask_is_its_id_and_text() {
        let a = parts("```ask push\nPush to main?\n- yes\n```\n");
        let b = parts("```ask push\nPush to main?\n- yes\n```\n\nMore prose.\n");
        let c = parts("```ask push\nPush to main now?\n- yes\n```\n");
        assert_eq!(a.asks.len(), 1);
        assert_eq!(a.asks[0].key, "push");
        assert_eq!(a.asks, b.asks);
        assert_ne!(a.asks[0].digest, c.asks[0].digest);
    }

    #[test]
    fn a_heading_is_its_title_and_what_it_says() {
        let a = parts("## Where we stand\n\nBuilding.\n\n## Next\n\nShip.\n");
        let b = parts("## Where we stand\n\nBuilding.\n\n## Next\n\nShip it.\n");
        assert_eq!(a.headings.len(), 2);
        assert_eq!(a.headings[0].key, "Where we stand");
        assert_eq!(a.headings[0], b.headings[0]);
        assert_ne!(a.headings[1], b.headings[1]);
    }

    #[test]
    fn repeated_titles_stay_apart() {
        let p = parts("## Notes\n\nOne.\n\n## Notes\n\nOne.\n");
        assert_ne!(p.headings[0].key, p.headings[1].key);
    }

    #[test]
    fn minting_twice_gives_two_identities() {
        let p = parts("```ask q\nShip?\n```\n");
        assert_ne!(mint(&p.asks[0]), mint(&p.asks[0]));
    }
}
