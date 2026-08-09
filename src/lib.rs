//! Perl parser plugin — full-parse mode.
//!
//! Handles `.pl`, `.pm`, `.t` files.
//! The plugin parses source with Tree-sitter inside Rust/Wasm.
//!
//! Semantic model:
//! - `package_statement`  → class-like (Perl OO: each package is a class)
//! - `sub_declaration`    → method-like (subroutine / method)
//! - Labels: package → package name; sub → subroutine name.

use intentumdiff_plugin_sdk::ts_convert::convert_semantic_classed;
use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct PerlParser;

const TRIVIA: &[&str] = &["comment", "line_comment"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "source_file",
    // Package (class-like in OO Perl / Moose / Moo)
    "package_statement",
    "package_declaration",
    // Imports
    "use_statement",
    "use_declaration",
    "no_statement",
    "require_expression",
    "require_statement",
    // Subroutine definitions (method-like)
    "sub_declaration",
    "subroutine_declaration_statement",
    "named_sub",
    "anonymous_sub",
    "method_declaration",
    // Variable declarations
    "my_var_declaration",
    "our_var_declaration",
    "local_var_declaration",
    "state_var_declaration",
    "my_declaration",
    "our_declaration",
    "local_declaration",
    // Assignments
    "assignment_expression",
    "compound_assignment_expression",
    // Control flow
    "if_statement",
    "unless_statement",
    "elsif_clause",
    "else_clause",
    "while_statement",
    "until_statement",
    "do_while_statement",
    "for_statement",
    "foreach_statement",
    "given_statement",
    "when_statement",
    "default_statement",
    "last_statement",
    "next_statement",
    "redo_statement",
    "return_statement",
    "die_statement",
    "exit_statement",
    // Error handling
    "eval_expression",
    "do_block",
    "block",
    // Statements (tree-sitter-perl wraps statement-level expressions in expression_statement;
    // without it every sub body pruned to an empty block and content edits hashed style-only)
    "expression_statement",
    // Calls (tree-sitter-perl kinds: print/say etc. are funcop/listop call expressions)
    "func0op_call_expression",
    "funcop_call_expression",
    "listop_call_expression",
    "function_call_expression",
    "ambiguous_function_call_expression",
    "method_call_expression",
    "call_expression",
    "method_call_expression",
    "function_call",
    // Expressions
    "binary_expression",
    "unary_expression",
    "ternary_expression",
    "string_operation",
    "regex_match",
    "regex_substitution",
    // Data (tree-sitter-perl literal kinds)
    "string_content",
    "string_literal",
    "interpolated_string_literal",
    "quoted_word_list",
    "number",
    "version",
    "string",
    "interpolated_string_expression",
    "heredoc",
    "heredoc_body",
    "array_ref",
    "hash_ref",
    "array_slice",
    "hash_slice",
    // Variables
    "scalar_variable",
    "array_variable",
    "hash_variable",
    "glob_variable",
    // Names
    "identifier",
    "package_name",
    "bare_word",
    // Misc
    "use_constant",
    "begin_block",
    "end_block",
    "init_block",
    "check_block",
    "unitcheck_block",
    "destroy_block",
    "autoload",
    "attribute",
];

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn is_class_like(node_type: &str) -> bool {
    matches!(node_type, "package_statement" | "package_declaration")
}

fn is_method_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "sub_declaration" | "named_sub" | "method_declaration" | "subroutine_declaration_statement"
    )
}

fn label_for(node: &CstNode) -> String {
    // A `block` / `do_block` is a body CONTAINER, identified structurally — never by source
    // text. An EMPTY `{ }` has no named children so tree-sitter yields a leaf; labelling it
    // with its text made a trivial-body -> real-body edit flip the label (leaf-text ->
    // structural), keeping a redundant parent MODIFICATION on top of the real body ADDITION
    // and blocking routing (issue #62 / #57 — same fix as elixir do_block).
    if matches!(node.node_type.as_str(), "block" | "do_block") {
        return node.node_type.clone();
    }
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    match node.node_type.as_str() {
        "package_statement" | "package_declaration" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "package_name" | "identifier" | "bare_word"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "sub_declaration"
        | "named_sub"
        | "method_declaration"
        | "subroutine_declaration_statement" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "identifier" | "name" | "bare_word" | "bareword"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
            return "(anonymous)".to_string();
        }
        "use_statement" | "use_declaration" | "no_statement" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "package_name" | "identifier" | "bare_word"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "require_expression" | "require_statement" => {
            if let Some(first) = node.children.first() {
                return first.text_or_empty().to_string();
            }
        }
        "call_expression"
        | "function_call"
        | "function_call_expression"
        | "ambiguous_function_call_expression"
        | "func0op_call_expression"
        | "func1op_call_expression"
        | "coderef_call_expression" => {
            if let Some(first) = node.children.first() {
                return first.text_or_empty().to_string();
            }
        }
        // Literal-ish nodes label with their captured source text so edits to them are
        // visible to the matcher (without this a changed string hashed tree-identical
        // -> style-only).
        "string_literal" | "interpolated_string_literal" | "command_string"
        | "quoted_word_list" | "string_content" | "assignment_expression" => {
            let text = node.text_or_empty();
            if !text.is_empty() {
                return text.chars().take(120).collect();
            }
        }
        "method_call_expression" => {
            // `$object->method(args)` — label as "object->method"
            let parts: Vec<&str> = node
                .children
                .iter()
                .take(3)
                .filter(|c| {
                    matches!(
                        c.node_type.as_str(),
                        "scalar_variable" | "identifier" | "bare_word" | "method_name"
                    )
                })
                .map(|c| c.text_or_empty())
                .collect();
            if !parts.is_empty() {
                return parts.join("->");
            }
        }
        "my_var_declaration"
        | "our_var_declaration"
        | "local_var_declaration"
        | "state_var_declaration"
        | "my_declaration"
        | "our_declaration"
        | "local_declaration" => {
            for child in &node.children {
                if matches!(
                    child.node_type.as_str(),
                    "scalar_variable" | "array_variable" | "hash_variable" | "identifier"
                ) {
                    return child.text_or_empty().to_string();
                }
            }
        }
        _ => {}
    }
    for child in &node.children {
        if matches!(child.node_type.as_str(), "identifier" | "bare_word") {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|t| TRIVIA.contains(&t),
        &is_semantic,
        &is_class_like,
        &is_method_like,
        &label_for,
    )
}

fn node_to_cst(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    let children: Vec<CstNode> = (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .map(|child| node_to_cst(child, source))
        .collect();

    // Literal-ish nodes keep their source text even when they have named children: string
    // innards are largely UNNAMED tokens (invisible to named_children), so without this the
    // content of an interpolated string or assignment vanished and edits to it hashed
    // tree-identical (issue #23).
    let keep_text = children.is_empty()
        || matches!(
            node.kind(),
            "string_literal"
                | "interpolated_string_literal"
                | "command_string"
                | "quoted_word_list"
                | "string_content"
                | "number"
                | "assignment_expression"
        );
    let text = if keep_text {
        Some(
            node.utf8_text(source)
                .unwrap_or("")
                .chars()
                .take(4096)
                .collect(),
        )
    } else {
        None
    };

    CstNode {
        node_type: node.kind().to_string(),
        named: node.is_named(),
        text,
        start_line: node.start_position().row as u32,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_col: node.end_position().column as u32,
        children,
    }
}

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = ts_parser_perl::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load perl grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{\"error\":\"{}\"}}"#, e),
    };
    let mut memo = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for PerlParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "perl".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".pl") || lower.ends_with(".pm") || lower.ends_with(".t") {
            return "perl".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["perl".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "sub greet {\n    my $name = shift;\n    print \"Hello, $name\\n\";\n}\n\nsub add {\n    my ($a, $b) = @_;\n    return $a + $b;\n}\n".to_string(),
            new: "use strict;\nuse warnings;\n\nsub greet {\n    my ($name) = @_;\n    print \"Hello, ${name}!\\n\";\n}\n\nsub add {\n    my ($x, $y) = @_;\n    return $x + $y;\n}\n".to_string(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    // Guards issue #23: the kind list once named a DIFFERENT perl grammar's node types, so sub
    // bodies pruned to empty blocks and body edits hashed style-only (invisible). These pin the
    // real ts-parser-perl kinds and the content-bearing labels.
    #[test]
    fn sub_body_statements_and_string_content_survive_conversion() {
        let source = "sub g {
    my ($name) = @_;
    print \"Hello, $name\n\";
}
";
        let cst = parse_source(source).expect("parse");
        fn find<'a>(node: &'a CstNode, kind: &str) -> Option<&'a CstNode> {
            if node.node_type == kind {
                return Some(node);
            }
            node.children.iter().find_map(|child| find(child, kind))
        }
        let statement = find(&cst, "expression_statement").expect("sub body statements kept");
        assert!(find(statement, "assignment_expression").is_some() || find(&cst, "assignment_expression").is_some());
        let content = find(&cst, "string_content").expect("string content kept");
        assert!(
            content.text_or_empty().contains("Hello"),
            "string_content must carry its source text, got {:?}",
            content.text_or_empty()
        );
    }

    #[test]
    fn changed_string_content_changes_the_semantic_label() {
        let old = parse_source("sub g {
    print \"Hello\n\";
}
").expect("parse old");
        let new = parse_source("sub g {
    print \"Goodbye\n\";
}
").expect("parse new");
        fn label_of(node: &CstNode, kind: &str) -> Option<String> {
            if node.node_type == kind {
                return Some(label_for(node));
            }
            node.children.iter().find_map(|child| label_of(child, kind))
        }
        let old_label = label_of(&old, "string_content").expect("old literal");
        let new_label = label_of(&new, "string_content").expect("new literal");
        assert_ne!(old_label, new_label, "content edits must be visible to the matcher");
        assert!(old_label.contains("Hello") && new_label.contains("Goodbye"));
    }

    #[test]
    fn empty_and_filled_blocks_share_a_structural_label() {
        // Issue #62/#57: an empty `sub f { }` body is a leaf; labelling it with its text made
        // a trivial-body -> real-body edit flip the label and keep a redundant parent
        // modification. Both empty and filled blocks label as their node_type.
        fn block_labels(source: &str) -> Vec<String> {
            let cst = parse_source(source).expect("parse");
            fn walk(node: &CstNode, out: &mut Vec<String>) {
                if node.node_type == "block" {
                    out.push(label_for(node));
                }
                for child in &node.children {
                    walk(child, out);
                }
            }
            let mut out = Vec::new();
            walk(&cst, &mut out);
            out
        }
        for label in block_labels("sub f {\n}\n") {
            assert_eq!(label, "block", "empty block must be structural");
        }
        for label in block_labels("sub f {\n    print \"x\";\n}\n") {
            assert_eq!(label, "block", "filled block must be structural");
        }
    }
}

export!(PerlParser);
