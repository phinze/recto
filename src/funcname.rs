//! Synthesize git-style xfuncname text for hunk headers.
//!
//! jj's `diff --git` output never includes the trailing function-context
//! string after the closing `@@`, so hunks read with no scope. We fill that
//! gap by parsing the post-image with tree-sitter and finding the smallest
//! enclosing definition that contains the hunk's starting line.
//!
//! Per-language for now: Go only. The next language to land here will
//! motivate a real trait.

use tree_sitter::{Node, Parser, Point};

/// Smallest enclosing `func`/method signature for `line` (1-based) in a Go
/// source file. Returns the leading-space-prefixed string that callers can
/// concatenate onto the closing `@@`, matching git's emitted format
/// (` func extractHTTPPort(...) (int64, bool) {`).
pub fn go_enclosing(content: &str, line: u32) -> Option<String> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_go::LANGUAGE.into()).ok()?;
    let tree = parser.parse(content, None)?;

    let row = line.saturating_sub(1) as usize;
    let target = Point::new(row, 0);

    let mut best: Option<Node> = None;
    walk(tree.root_node(), target, &mut best);

    let node = best?;
    let body = node.child_by_field_name("body")?;
    let raw = content.get(node.start_byte()..body.start_byte())?;
    Some(format!(" {} {{", normalize(raw)))
}

fn walk<'a>(node: Node<'a>, target: Point, best: &mut Option<Node<'a>>) {
    if target < node.start_position() || target >= node.end_position() {
        return;
    }
    if matches!(
        node.kind(),
        "function_declaration" | "method_declaration" | "func_literal"
    ) {
        *best = Some(node);
    }
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        walk(child, target, best);
    }
}

/// Multi-line Go signatures (long parameter lists wrapped across lines) get
/// flattened so the hunk header stays a single line.
fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"package main

import "fmt"

func short() {
    fmt.Println("hi")
}

func extractHTTPPort(spec *Sandbox) (int64, bool) {
    if spec == nil {
        return 0, false
    }
    return 8080, true
}

type Server struct{}

func (s *Server) Handle(req *Request) error {
    if req == nil {
        return nil
    }
    return nil
}
"#;

    #[test]
    fn finds_top_level_func() {
        // Line 10 is inside extractHTTPPort
        let got = go_enclosing(SAMPLE, 10).unwrap();
        assert_eq!(got, " func extractHTTPPort(spec *Sandbox) (int64, bool) {");
    }

    #[test]
    fn finds_method_with_receiver() {
        // Line 19 is inside (s *Server) Handle
        let got = go_enclosing(SAMPLE, 19).unwrap();
        assert_eq!(got, " func (s *Server) Handle(req *Request) error {");
    }

    #[test]
    fn returns_none_outside_any_func() {
        // Line 3 is the import statement
        assert!(go_enclosing(SAMPLE, 3).is_none());
    }

    #[test]
    fn flattens_wrapped_signatures() {
        let src =
            "package x\n\nfunc Wide(\n\ta int,\n\tb int,\n) (int, error) {\n\treturn 0, nil\n}\n";
        // Line 7 is inside the body
        let got = go_enclosing(src, 7).unwrap();
        assert_eq!(got, " func Wide( a int, b int, ) (int, error) {");
    }
}
