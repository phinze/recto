//! Commit-graph layout.
//!
//! recto used to let jj draw the graph and parsed the glyphs back out. That
//! made the panel depend on jj's *rendering* rather than on a template we
//! define, which is the one kind of coupling jj otherwise lets you avoid: ask
//! for `parents` and topology arrives as data. So the drawing happens here,
//! from ids and edges, which also makes it a pure function worth testing.
//!
//! The layout is the standard lane assignment: every rev occupies a column,
//! a column stays alive while something still expects it as a parent, and
//! columns collapse when lines converge.

/// One rev's worth of graph, split around the node so the caller supplies its
/// own glyph and colour. A row renders as `left + <node> + right`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Row {
    /// Lane glyphs left of this rev's node.
    pub left: String,
    /// Lanes that continue to the right of this rev's node.
    pub right: String,
    /// Connector drawn *above* this rev when lanes collapse into it, e.g.
    /// `├─╯`. It renders between the last row of the spur and the rev the
    /// lines meet at, which is the row it describes. `None` when nothing
    /// converges here.
    pub join: Option<String>,
}

/// Input edge list. `id` is the rev, `parents` its parent ids; parents outside
/// the window are ignored, which is what makes the graph terminate at the
/// bottom of the slice instead of running off it.
pub struct Node<'a> {
    pub id: &'a str,
    pub parents: &'a [String],
}

/// Order revs children-before-parents, the direction jj logs read.
///
/// Deliberately not "trust the order jj handed us": with `--no-graph` that
/// order is chronological, and a rebase can hand a child an older timestamp
/// than its parent, which would draw an edge pointing the wrong way up the
/// panel. `priority` (normally `@`) breaks ties so the line you're working on
/// leads, matching jj's own `log-graph-prioritize`.
pub fn topo_order(nodes: &[Node<'_>], priority: Option<&str>) -> Vec<usize> {
    let index: std::collections::HashMap<&str, usize> =
        nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();

    // Edges kept only where both ends are in the window.
    let parents: Vec<Vec<usize>> = nodes
        .iter()
        .map(|n| {
            n.parents
                .iter()
                .filter_map(|p| index.get(p.as_str()).copied())
                .collect()
        })
        .collect();

    // A rev is ready once every child of it has been emitted.
    let mut pending_children = vec![0usize; nodes.len()];
    for ps in &parents {
        for &p in ps {
            pending_children[p] += 1;
        }
    }

    let prio = priority.and_then(|p| index.get(p).copied());
    let mut emitted = vec![false; nodes.len()];
    let mut out = Vec::with_capacity(nodes.len());

    for _ in 0..nodes.len() {
        // Ready set, resolved by: the priority rev, then input order. Input
        // order is jj's chronological order, so among genuinely unordered
        // revs the panel still reads newest-first.
        let mut best: Option<usize> = None;
        for i in 0..nodes.len() {
            if emitted[i] || pending_children[i] > 0 {
                continue;
            }
            if Some(i) == prio {
                best = Some(i);
                break;
            }
            if best.is_none() {
                best = Some(i);
            }
        }
        // A cycle can't happen in a commit DAG, but a malformed window
        // shouldn't hang the UI: fall back to the first unemitted rev.
        let pick = match best.or_else(|| (0..nodes.len()).find(|&i| !emitted[i])) {
            Some(p) => p,
            None => break,
        };
        emitted[pick] = true;
        for &p in &parents[pick] {
            pending_children[p] = pending_children[p].saturating_sub(1);
        }
        out.push(pick);
    }

    out
}

/// Assign lanes and render the glyphs, given rows already in topological
/// order. Returns one `Row` per input rev, in the same order.
pub fn lay_out(ordered: &[Node<'_>]) -> Vec<Row> {
    // Each live lane is waiting for one rev id to show up.
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(ordered.len());

    for node in ordered {
        // The lane that expected this rev, or a fresh one. Reusing a hole
        // left by a collapsed lane keeps the graph from drifting rightwards
        // across a long history.
        let mine = match lanes.iter().position(|l| l.as_deref() == Some(node.id)) {
            Some(i) => i,
            None => match lanes.iter().position(|l| l.is_none()) {
                Some(i) => i,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            },
        };

        // Any *other* lane waiting for the same rev is a line converging here.
        let converging: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(i, l)| *i != mine && l.as_deref() == Some(node.id))
            .map(|(i, _)| i)
            .collect();
        for &i in &converging {
            lanes[i] = None;
        }

        // Drop lanes that just died off the right edge *before* drawing, or a
        // converged spur leaves a blank column trailing every row below it.
        // Never trims past this rev's own lane, whose index the render needs.
        while lanes.len() > mine + 1 && lanes.last().is_some_and(|l| l.is_none()) {
            lanes.pop();
        }

        let left = render_lanes(&lanes, 0..mine);
        let right = render_lanes(&lanes, mine + 1..lanes.len());

        // This rev's own lane now waits for its first in-window parent;
        // further parents (a merge) open lanes of their own.
        lanes[mine] = node.parents.first().cloned();
        for extra in node.parents.iter().skip(1) {
            match lanes.iter().position(|l| l.is_none()) {
                Some(i) => lanes[i] = Some(extra.clone()),
                None => lanes.push(Some(extra.clone())),
            }
        }

        let join = converging
            .iter()
            .max()
            .map(|&far| render_collapse(mine, far, &lanes));

        rows.push(Row { left, right, join });
        trim_trailing_lanes(&mut lanes);
    }

    rows
}

fn render_lanes(lanes: &[Option<String>], range: std::ops::Range<usize>) -> String {
    let mut s = String::new();
    for i in range {
        s.push_str(if lanes.get(i).is_some_and(|l| l.is_some()) {
            "│ "
        } else {
            "  "
        });
    }
    s
}

/// The row drawn under a rev that two or more lines reach: `├─╯` for a lane
/// folding in from the right, widened for whatever the gap is.
fn render_collapse(mine: usize, far: usize, lanes: &[Option<String>]) -> String {
    let mut s = String::new();
    for i in 0..mine {
        s.push_str(if lanes.get(i).is_some_and(|l| l.is_some()) {
            "│ "
        } else {
            "  "
        });
    }
    s.push('├');
    for _ in (mine + 1)..far {
        s.push_str("──");
    }
    s.push_str("─╯");
    s
}

/// Drop dead lanes off the right edge so the graph narrows again once a spur
/// is done, instead of carrying blank columns to the bottom of the panel.
fn trim_trailing_lanes(lanes: &mut Vec<Option<String>>) {
    while lanes.last().is_some_and(|l| l.is_none()) {
        lanes.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes<'a>(spec: &'a [(&'a str, Vec<String>)]) -> Vec<Node<'a>> {
        spec.iter()
            .map(|(id, parents)| Node { id, parents })
            .collect()
    }

    fn p(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn linear_history_draws_no_lanes() {
        let spec = [("c", p(&["b"])), ("b", p(&["a"])), ("a", p(&[]))];
        let rows = lay_out(&nodes(&spec));
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(row.left, "");
            assert_eq!(row.right, "");
            assert_eq!(row.join, None);
        }
    }

    #[test]
    fn a_spur_gets_its_own_lane_and_collapses_at_the_fork() {
        // @'s line is c -> b -> base; a spur s1 -> base rejoins at the bottom.
        let spec = [
            ("c", p(&["b"])),
            ("b", p(&["base"])),
            ("s1", p(&["base"])),
            ("base", p(&[])),
        ];
        let rows = lay_out(&nodes(&spec));
        // c and b sit in lane 0 with nothing beside them yet.
        assert_eq!(rows[0].left, "");
        // The spur opens a second lane, so it renders indented.
        assert_eq!(rows[2].left, "│ ");
        // base is reached by both lines, so the spur folds in under it.
        assert_eq!(rows[3].join.as_deref(), Some("├─╯"));
        assert_eq!(rows[3].left, "");
    }

    #[test]
    fn topo_order_beats_a_rebased_childs_stale_timestamp() {
        // Input order is chronological and puts the parent first, which is
        // what a rebase can do to a child's timestamp. Children must still
        // come first or the graph draws an edge pointing the wrong way.
        let spec = [("parent", p(&[])), ("child", p(&["parent"]))];
        let n = nodes(&spec);
        let order = topo_order(&n, None);
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn priority_rev_leads_among_equals() {
        // Two independent heads; @ should come first regardless of input order.
        let spec = [
            ("other", p(&["base"])),
            ("wc", p(&["base"])),
            ("base", p(&[])),
        ];
        let n = nodes(&spec);
        assert_eq!(topo_order(&n, Some("wc"))[0], 1);
        assert_eq!(topo_order(&n, Some("other"))[0], 0);
    }

    #[test]
    fn parents_outside_the_window_just_end_the_line() {
        // `a`'s parent isn't in the slice; the lane closes rather than the
        // layout panicking or inventing a row for it.
        let spec = [("a", p(&["off-window"]))];
        let rows = lay_out(&nodes(&spec));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].left, "");
        let n = nodes(&spec);
        assert_eq!(topo_order(&n, None), vec![0]);
    }

    #[test]
    fn a_merge_opens_a_lane_and_the_graph_narrows_again() {
        // m has two parents; both lines run until they meet at base.
        let spec = [
            ("m", p(&["l", "r"])),
            ("l", p(&["base"])),
            ("r", p(&["base"])),
            ("base", p(&[])),
        ];
        let n = nodes(&spec);
        let order = topo_order(&n, None);
        let ordered: Vec<Node<'_>> = order
            .iter()
            .map(|&i| Node {
                id: n[i].id,
                parents: n[i].parents,
            })
            .collect();
        let rows = lay_out(&ordered);
        // The merge itself is in lane 0 with no lane yet to its right.
        assert_eq!(rows[0].right, "");
        // One of the two sides renders indented while both are live.
        assert!(rows.iter().any(|r| r.left == "│ "));
        // And the two lines fold back together at base.
        assert!(rows.last().is_some_and(|r| r.join.is_some()));
        // Nothing is left hanging to the right once it collapses.
        assert_eq!(rows.last().unwrap().right, "");
    }
}
