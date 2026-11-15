//! A repository's work as a stack: layers of changes, each a PR or one meant
//! to be, bounded by bookmarks.
//!
//! The stack is computed from the commits in view, never stored. A bookmark
//! on a commit makes it the head of a layer, and the layer runs down to the
//! commit above the next bookmark. Commits above the topmost bookmark are the
//! work not yet given a name. A stack with no bookmarks is one layer, which is
//! exactly the single range Recto has always shown.

use crate::backend::Commit;

/// One layer of a stack, newest commit first.
pub struct Layer<'a> {
    /// The bookmark naming the layer, or `None` for unnamed work on top.
    pub name: Option<&'a str>,
    pub commits: &'a [Commit],
}

impl<'a> Layer<'a> {
    pub fn head(&self) -> &'a Commit {
        &self.commits[0]
    }

    pub fn bottom(&self) -> &'a Commit {
        &self.commits[self.commits.len() - 1]
    }

    /// Every commit in the layer is already on trunk.
    pub fn landed(&self) -> bool {
        self.commits.iter().all(|c| c.landed)
    }

    /// How a view names this layer, so the name outlives a rewrite of its
    /// head the way a change view does.
    pub fn view(&self) -> String {
        format!("layer:{}", self.head().target())
    }

    pub fn label(&self) -> String {
        match self.name {
            Some(name) => name.to_string(),
            None => "unpushed".to_string(),
        }
    }

    pub fn contains(&self, commit: &Commit) -> bool {
        self.commits.iter().any(|c| c.id == commit.id)
    }
}

/// Split commits, newest first, into layers, newest first.
pub fn layers(commits: &[Commit]) -> Vec<Layer<'_>> {
    let mut starts: Vec<usize> = commits
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.bookmarks.is_empty())
        .map(|(i, _)| i)
        .collect();
    if starts.first() != Some(&0) && !commits.is_empty() {
        starts.insert(0, 0);
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(commits.len());
            Layer {
                name: commits[start].bookmarks.first().map(String::as_str),
                commits: &commits[start..end],
            }
        })
        .collect()
}

/// What the diff shows: the whole stack, one layer, or one change.
pub enum View<'a> {
    Stack,
    Layer(Layer<'a>),
    Change(&'a Commit),
}

/// Find the view a spec names among the stack's commits. A spec is empty for
/// the whole stack, `layer:CHANGE` for the layer headed by that change, or a
/// change. A change is named by its target, by a unique prefix of its change
/// id (what an agent types), or by a commit id, for specs picked before views
/// named changes. A spec that names nothing here is the whole stack.
pub fn view<'a>(commits: &'a [Commit], spec: Option<&str>) -> View<'a> {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return View::Stack;
    };
    if let Some(head) = spec.strip_prefix("layer:") {
        let Some(head) = find(commits, head) else {
            return View::Stack;
        };
        return layers(commits)
            .into_iter()
            .find(|l| l.head().id == head.id)
            .map_or(View::Stack, View::Layer);
    }
    find(commits, spec).map_or(View::Stack, View::Change)
}

fn find<'a>(commits: &'a [Commit], name: &str) -> Option<&'a Commit> {
    if let Some(c) = commits.iter().find(|c| c.target() == name || c.id == name) {
        return Some(c);
    }
    let mut prefixed = commits.iter().filter(|c| c.change.starts_with(name));
    let first = prefixed.next()?;
    prefixed.next().is_none().then_some(first)
}

/// The changes in reading order, oldest first, for stepping through a stack
/// one change at a time.
pub fn reading_order(commits: &[Commit]) -> impl Iterator<Item = &Commit> {
    commits.iter().rev()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(id: &str, bookmarks: &[&str], landed: bool) -> Commit {
        Commit {
            id: format!("{id}000"),
            change: id.to_string(),
            divergent: false,
            bookmarks: bookmarks.iter().map(|b| b.to_string()).collect(),
            summary: id.to_string(),
            landed,
            mine: true,
            empty: false,
            added: 0,
            removed: 0,
            when: String::new(),
        }
    }

    #[test]
    fn bookmarks_split_a_stack_into_layers_with_unnamed_work_on_top() {
        let commits = vec![
            commit("e", &[], false),
            commit("d", &["second"], false),
            commit("c", &[], false),
            commit("b", &["first"], true),
            commit("a", &[], true),
        ];
        let split = layers(&commits);
        let shape: Vec<(String, Vec<&str>, bool)> = split
            .iter()
            .map(|l| {
                (
                    l.label(),
                    l.commits.iter().map(|c| c.change.as_str()).collect(),
                    l.landed(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                ("unpushed".to_string(), vec!["e"], false),
                ("second".to_string(), vec!["d", "c"], false),
                ("first".to_string(), vec!["b", "a"], true),
            ]
        );
    }

    #[test]
    fn a_stack_without_bookmarks_is_one_layer() {
        let commits = vec![commit("b", &[], false), commit("a", &[], false)];
        let split = layers(&commits);
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].commits.len(), 2);
        assert!(layers(&[]).is_empty());
    }

    #[test]
    fn a_view_spec_finds_its_layer_or_change_or_falls_back_to_the_stack() {
        let commits = vec![
            commit("c", &["top"], false),
            commit("b", &[], false),
            commit("a", &["base"], false),
        ];
        assert!(matches!(view(&commits, None), View::Stack));
        assert!(matches!(view(&commits, Some("")), View::Stack));
        assert!(matches!(view(&commits, Some("layer:c")), View::Layer(l) if l.commits.len() == 2));
        assert!(matches!(view(&commits, Some("b")), View::Change(c) if c.change == "b"));
        assert!(matches!(view(&commits, Some("b000")), View::Change(c) if c.change == "b"));
        assert!(matches!(view(&commits, Some("layer:b")), View::Stack));
        assert!(matches!(view(&commits, Some("gone")), View::Stack));

        let long = vec![
            commit("kxyzabcd", &[], false),
            commit("kxyqrstu", &[], false),
        ];
        assert!(matches!(view(&long, Some("kxyz")), View::Change(c) if c.change == "kxyzabcd"));
        assert!(
            matches!(view(&long, Some("kxy")), View::Stack),
            "an ambiguous prefix names nothing"
        );
    }
}
