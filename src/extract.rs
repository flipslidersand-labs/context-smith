use anyhow::{Context, Result};
use std::path::Path;

use crate::path_kind;
use crate::{Language, SymbolKind};

pub(crate) struct SymbolExtractor;

impl SymbolExtractor {
    pub(crate) fn extract(
        path: &Path,
        source: &str,
        lang: Language,
    ) -> Result<Vec<(String, SymbolKind)>> {
        type Collect = fn(tree_sitter::Node, &[u8], &Path) -> Vec<(String, SymbolKind)>;
        // (grammar, label for error messages, collector)
        let (grammar, name, collect): (tree_sitter::Language, &str, Collect) = match lang {
            Language::Rust => (
                tree_sitter_rust::LANGUAGE.into(),
                "Rust",
                collect_rust_symbols,
            ),
            Language::Python => (
                tree_sitter_python::LANGUAGE.into(),
                "Python",
                collect_python_symbols,
            ),
            Language::Go => (tree_sitter_go::LANGUAGE.into(), "Go", collect_go_symbols),
            Language::TypeScript => {
                // TSX is not a superset of TS: `<T>x` assertions and `<T>(x) => x` parse as JSX
                // there. Use the TSX grammar only for `.tsx`; `.ts`/`.mts`/`.cts` use plain TS.
                let grammar = if path.extension().and_then(|e| e.to_str()) == Some("tsx") {
                    tree_sitter_typescript::LANGUAGE_TSX
                } else {
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT
                };
                (grammar.into(), "TypeScript", collect_ecma_symbols)
            }
            Language::JavaScript => (
                tree_sitter_javascript::LANGUAGE.into(),
                "JavaScript",
                collect_ecma_symbols,
            ),
            Language::Ruby => (
                tree_sitter_ruby::LANGUAGE.into(),
                "Ruby",
                collect_ruby_symbols,
            ),
            Language::Unknown => return Ok(vec![]),
        };
        let tree = Self::parse(path, source, grammar, name)?;
        Ok(collect(tree.root_node(), source.as_bytes(), path))
    }

    /// Parse `source` with `grammar`; `name` only labels error messages.
    fn parse(
        path: &Path,
        source: &str,
        grammar: tree_sitter::Language,
        name: &str,
    ) -> Result<tree_sitter::Tree> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar)
            .with_context(|| format!("Failed to load {name} grammar for {:?}", path))?;
        parser
            .parse(source, None)
            .with_context(|| format!("{name} parse failed"))
    }
}

/// Maximum syntax-tree depth the recursive symbol walkers descend. Deeper branches are
/// skipped so a pathologically nested file cannot overflow the stack (#417). Real code
/// nests well below this.
const MAX_WALK_DEPTH: usize = 500;

/// `path` is a name from the scanned repo, so control characters are escaped
/// before it reaches a terminal (#566).
fn truncation_warning(path: &Path) -> String {
    format!(
        "warn: {}: nesting deeper than {MAX_WALK_DEPTH}, deeper nodes skipped",
        path_kind::escape_control(&path.to_string_lossy())
    )
}

fn warn_if_truncated(path: &Path, truncated: bool) {
    if truncated {
        eprintln!("{}", truncation_warning(path));
    }
}

fn node_text<'a>(node: tree_sitter::Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.byte_range()]).unwrap_or("")
}

fn collect_rust_symbols(
    root: tree_sitter::Node,
    src: &[u8],
    path: &Path,
) -> Vec<(String, SymbolKind)> {
    let mut out = Vec::new();
    let mut truncated = false;
    traverse_rust(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn traverse_rust(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
    match node.kind() {
        "function_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function));
            }
        }
        "struct_item" | "enum_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct));
            }
        }
        "trait_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class));
            }
        }
        "type_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct));
            }
        }
        "impl_item" => {
            if let Some(ty) = node.child_by_field_name("type") {
                out.push((node_text(ty, src).to_owned(), SymbolKind::Impl));
            }
        }
        "use_declaration" => {
            // Take the `argument` field so visibility modifiers (`pub`,
            // `pub(crate)`, `pub(in path)`) are excluded. A missing/empty argument
            // (syntax-error recovery such as `pub use ;`) is not recorded.
            if let Some(arg) = node.child_by_field_name("argument") {
                let text = node_text(arg, src).trim();
                if !text.is_empty() {
                    out.push((text.to_owned(), SymbolKind::Import));
                }
            }
        }
        "mod_item" => {
            // `mod foo;` (no body) at file level declares a child module file.
            // Recorded as `mod foo` (never a valid `use` argument) so the resolver can
            // tell it apart. Inline `mod foo { .. }` and nested declarations are skipped.
            let top_level = node.parent().is_some_and(|p| p.kind() == "source_file");
            if top_level && node.child_by_field_name("body").is_none() {
                if let Some(name) = node.child_by_field_name("name") {
                    out.push((format!("mod {}", node_text(name, src)), SymbolKind::Import));
                }
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            traverse_rust(child, src, out, depth + 1, truncated);
        }
    }
}

fn collect_python_symbols(
    root: tree_sitter::Node,
    src: &[u8],
    path: &Path,
) -> Vec<(String, SymbolKind)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_python_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_python_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
    match node.kind() {
        "function_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function));
            }
        }
        "class_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class));
            }
        }
        "import_statement" | "import_from_statement" => {
            out.push((node_text(node, src).to_owned(), SymbolKind::Import));
        }
        _ => {}
    }
    for i in 0..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            collect_python_node(child, src, out, depth + 1, truncated);
        }
    }
}

/// Symbol collector shared by TypeScript and JavaScript (TS is a superset of the
/// JS node kinds; unknown kinds are simply ignored).
fn collect_ecma_symbols(
    root: tree_sitter::Node,
    src: &[u8],
    path: &Path,
) -> Vec<(String, SymbolKind)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_ecma_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_ecma_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
    match node.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "function_signature"
        | "method_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function));
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class));
            }
        }
        // TypeScript-only type declarations map to Struct.
        "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct));
            }
        }
        // `const foo = () => {}` / `const foo = function () {}`
        "variable_declarator" => {
            if let (Some(name), Some(value)) = (
                node.child_by_field_name("name"),
                node.child_by_field_name("value"),
            ) {
                if matches!(
                    value.kind(),
                    "arrow_function" | "function" | "function_expression"
                ) {
                    out.push((node_text(name, src).to_owned(), SymbolKind::Function));
                }
            }
        }
        "import_statement" => {
            out.push((node_text(node, src).to_owned(), SymbolKind::Import));
        }
        // Re-exports (`export ... from './x'`) are dependencies; plain `export { a }` is not.
        "export_statement" => {
            if node.child_by_field_name("source").is_some() {
                out.push((node_text(node, src).to_owned(), SymbolKind::Import));
            }
        }
        // CommonJS `require('./x')` — recorded as an import for dependency resolution.
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                if node_text(func, src) == "require" {
                    out.push((node_text(node, src).to_owned(), SymbolKind::Import));
                }
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            collect_ecma_node(child, src, out, depth + 1, truncated);
        }
    }
}

fn collect_go_symbols(
    root: tree_sitter::Node,
    src: &[u8],
    path: &Path,
) -> Vec<(String, SymbolKind)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_go_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_go_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function));
            }
        }
        "type_declaration" => {
            for i in 0..node.child_count() as u32 {
                if let Some(spec) = node.child(i) {
                    if matches!(spec.kind(), "type_spec" | "type_alias") {
                        if let Some(name) = spec.child_by_field_name("name") {
                            // Use the spec's own row so grouped `type ( ... )` entries point at their definition.
                            out.push((node_text(name, src).to_owned(), SymbolKind::Struct));
                        }
                    }
                }
            }
        }
        "import_declaration" => {
            out.push((node_text(node, src).to_owned(), SymbolKind::Import));
        }
        _ => {}
    }
    for i in 0..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            collect_go_node(child, src, out, depth + 1, truncated);
        }
    }
}

fn collect_ruby_symbols(
    root: tree_sitter::Node,
    src: &[u8],
    path: &Path,
) -> Vec<(String, SymbolKind)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_ruby_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_ruby_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
    match node.kind() {
        "method" | "singleton_method" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function));
            }
        }
        "class" | "singleton_class" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class));
            }
        }
        "module" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct));
            }
        }
        // require / require_relative calls
        "call" => {
            if let Some(method) = node.child_by_field_name("method") {
                let m = node_text(method, src);
                // Receiver-less only: `Bundler.require(:default)` / `Kernel.require 'x'` are not imports.
                if (m == "require" || m == "require_relative")
                    && node.child_by_field_name("receiver").is_none()
                {
                    out.push((node_text(node, src).to_owned(), SymbolKind::Import));
                }
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() as u32 {
        if let Some(child) = node.child(i) {
            collect_ruby_node(child, src, out, depth + 1, truncated);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(syms: &[(String, SymbolKind)], kind: SymbolKind) -> Vec<&str> {
        syms.iter()
            .filter(|(_, k)| *k == kind)
            .map(|(n, _)| n.as_str())
            .collect()
    }

    /// #599: a path from the scanned repo must not reach stderr with raw control
    /// characters (ESC, BEL), even in the nesting-depth warning.
    #[test]
    fn truncation_warning_escapes_control_characters_in_the_path() {
        let msg = truncation_warning(Path::new("d\u{1b}[2J/a\u{7}b.rs"));
        assert!(!msg.chars().any(path_kind::is_ctrl), "{msg:?}");
        assert!(msg.contains("d\\x1b[2J/a\\x07b.rs"), "{msg:?}");
        assert!(msg.contains("nesting deeper than"), "{msg:?}");
    }

    // ---- SymbolExtractor ----

    #[test]
    fn extract_unknown_language_is_empty() {
        let out =
            SymbolExtractor::extract(Path::new("x.txt"), "anything", Language::Unknown).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn extract_rust_symbols() {
        let src = "use std::fmt;\nstruct S;\nenum E { A }\ntrait T {}\ntype Alias = u32;\nimpl S {}\nfn run() {}\n";
        let out = SymbolExtractor::extract(Path::new("a.rs"), src, Language::Rust).unwrap();
        assert_eq!(names(&out, SymbolKind::Function), vec!["run"]);
        assert_eq!(names(&out, SymbolKind::Struct), vec!["S", "E", "Alias"]);
        assert_eq!(names(&out, SymbolKind::Class), vec!["T"]);
        assert_eq!(names(&out, SymbolKind::Impl), vec!["S"]);
        // `use ` prefix and trailing `;` are trimmed.
        assert_eq!(out[0], ("std::fmt".to_string(), SymbolKind::Import));
        assert!(out.contains(&("run".to_string(), SymbolKind::Function)));
    }

    #[test]
    fn extract_rust_pub_use_reexports() {
        let src = "pub use crate::foo::Bar;\npub(crate) use crate::a::B;\npub(super) use super::c::D;\npub(in crate::x) use crate::x::E;\nuse std::fmt;\n";
        let out = SymbolExtractor::extract(Path::new("lib.rs"), src, Language::Rust).unwrap();
        assert_eq!(
            names(&out, SymbolKind::Import),
            vec![
                "crate::foo::Bar",
                "crate::a::B",
                "super::c::D",
                "crate::x::E",
                "std::fmt"
            ]
        );
    }

    #[test]
    fn extract_rust_use_without_argument_is_not_imported() {
        for src in ["pub use ;\n", "use ;\n", "pub(crate) use ;\nfn f() {}\n"] {
            let out = SymbolExtractor::extract(Path::new("a.rs"), src, Language::Rust).unwrap();
            assert!(
                names(&out, SymbolKind::Import).is_empty(),
                "empty import saved for {src:?}: {out:?}"
            );
        }
    }

    #[test]
    fn extract_rust_use_lookalikes_not_imports() {
        let src =
            "fn reuse() {}\n// use crate::commented::X;\nconst S: &str = \"use crate::s::Y;\";\n";
        let out = SymbolExtractor::extract(Path::new("a.rs"), src, Language::Rust).unwrap();
        assert!(names(&out, SymbolKind::Import).is_empty());
    }

    #[test]
    fn extract_rust_mod_declarations_only_without_body() {
        let src = "mod a;\npub mod b;\nmod inline { mod nested; }\nmod c { fn f() {} }\n";
        let out = SymbolExtractor::extract(Path::new("lib.rs"), src, Language::Rust).unwrap();
        assert_eq!(names(&out, SymbolKind::Import), vec!["mod a", "mod b"]);
    }

    #[test]
    fn extract_ts_angle_bracket_assertion_keeps_later_symbols() {
        let src = "const x = <Foo>y;\nconst f = <T>(v: T) => v;\nimport { a } from './a';\nfunction after() {}\n";
        let out = SymbolExtractor::extract(Path::new("a.ts"), src, Language::TypeScript).unwrap();
        // Under the TSX grammar `<T>(v: T) => v` is parsed as JSX and `f` is lost.
        assert!(names(&out, SymbolKind::Function).contains(&"f"));
        assert!(names(&out, SymbolKind::Function).contains(&"after"));
        assert!(!names(&out, SymbolKind::Import).is_empty());
    }

    /// `.tsx` uses the TSX grammar, so JSX parses cleanly and symbols inside it are found.
    #[test]
    fn extract_tsx_parses_jsx() {
        let jsx = "const el = <div>hi</div>;\nfunction App() { return <b>{[1].map(function inner() { return <i/>; })}</b>; }\n";
        let tsx = SymbolExtractor::extract(Path::new("a.tsx"), jsx, Language::TypeScript).unwrap();
        assert_eq!(names(&tsx, SymbolKind::Function), vec!["App"]);
    }

    #[test]
    fn extract_rust_empty_source() {
        let out = SymbolExtractor::extract(Path::new("a.rs"), "", Language::Rust).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn extract_python_symbols() {
        let src = "import os\nfrom a import b\nclass Foo:\n    def method(self):\n        pass\n\ndef top():\n    pass\n";
        let out = SymbolExtractor::extract(Path::new("a.py"), src, Language::Python).unwrap();
        assert_eq!(names(&out, SymbolKind::Class), vec!["Foo"]);
        assert_eq!(names(&out, SymbolKind::Function), vec!["method", "top"]);
        assert_eq!(names(&out, SymbolKind::Import).len(), 2);
    }

    #[test]
    fn extract_python_decorated_async_nested() {
        // One literal per line: a `\` line continuation would strip the indentation that the
        // nested definitions depend on.
        let src = concat!(
            "# def commented():\n",
            "x = \"def in_string(): pass\"\n",
            "@deco\n",
            "def decorated():\n",
            "    def inner():\n",
            "        pass\n",
            "    class InnerCls:\n",
            "        pass\n",
            "\n",
            "@deco2\n",
            "class Decorated:\n",
            "    async def am(self):\n",
            "        pass\n",
            "\n",
            "async def top_async():\n",
            "    pass\n",
        );
        let out = SymbolExtractor::extract(Path::new("a.py"), src, Language::Python).unwrap();
        let got: Vec<(&str, SymbolKind)> =
            out.iter().map(|(n, k)| (n.as_str(), k.clone())).collect();
        assert_eq!(
            got,
            vec![
                ("decorated", SymbolKind::Function),
                ("inner", SymbolKind::Function),
                ("InnerCls", SymbolKind::Class),
                ("Decorated", SymbolKind::Class),
                ("am", SymbolKind::Function),
                ("top_async", SymbolKind::Function),
            ]
        );
    }

    #[test]
    fn extract_python_import_forms() {
        let src = "import a.b as c\nfrom . import x\nfrom .. import y\nfrom a import (b,\n    c)\n\"\"\"\nimport in_doc\n\"\"\"\n# import in_comment\n";
        let out = SymbolExtractor::extract(Path::new("a.py"), src, Language::Python).unwrap();
        assert_eq!(
            names(&out, SymbolKind::Import),
            vec![
                "import a.b as c",
                "from . import x",
                "from .. import y",
                "from a import (b,\n    c)",
            ]
        );
    }

    #[test]
    fn extract_go_symbols() {
        let src = "package main\n\nimport \"fmt\"\n\ntype Server struct{}\n\nfunc (s *Server) Run() {}\n\nfunc main() { fmt.Println() }\n";
        let out = SymbolExtractor::extract(Path::new("a.go"), src, Language::Go).unwrap();
        assert_eq!(names(&out, SymbolKind::Struct), vec!["Server"]);
        assert_eq!(names(&out, SymbolKind::Function), vec!["Run", "main"]);
        assert_eq!(names(&out, SymbolKind::Import).len(), 1);
    }

    #[test]
    fn extract_go_grouped_type_declaration() {
        let src = "package p\n\ntype (\n    A struct{}\n    B interface{}\n    C = A\n)\n\ntype D struct{}\n";
        let out = SymbolExtractor::extract(Path::new("a.go"), src, Language::Go).unwrap();
        assert_eq!(names(&out, SymbolKind::Struct), vec!["A", "B", "C", "D"]);
    }

    #[test]
    fn extract_typescript_symbols() {
        let src = "import x from './x';\ninterface I {}\ntype T = string;\nenum E { A }\nabstract class Base {}\nclass C { m() {} }\nfunction f() {}\nconst g = () => 1;\n";
        let out = SymbolExtractor::extract(Path::new("a.ts"), src, Language::TypeScript).unwrap();
        assert_eq!(names(&out, SymbolKind::Struct), vec!["I", "T", "E"]);
        assert_eq!(names(&out, SymbolKind::Class), vec!["Base", "C"]);
        assert_eq!(names(&out, SymbolKind::Function), vec!["m", "f", "g"]);
        assert_eq!(names(&out, SymbolKind::Import).len(), 1);
    }

    #[test]
    fn extract_javascript_symbols_including_require() {
        let src = "const lib = require('./lib');\nconst h = function () {};\nfunction* gen() {}\nclass K {}\nconst plain = 5;\n";
        let out = SymbolExtractor::extract(Path::new("a.js"), src, Language::JavaScript).unwrap();
        assert_eq!(names(&out, SymbolKind::Import).len(), 1);
        assert_eq!(names(&out, SymbolKind::Class), vec!["K"]);
        let funcs = names(&out, SymbolKind::Function);
        assert!(funcs.contains(&"h") && funcs.contains(&"gen"), "{funcs:?}");
        assert!(!funcs.contains(&"plain"));
    }

    #[test]
    fn extract_ecma_export_from_is_import() {
        let src = "export * from './a';\nexport { b } from './b';\nexport * as ns from './c';\nexport { d };\nexport const e = 1;\n";
        for (path, lang) in [
            ("a.ts", Language::TypeScript),
            ("a.js", Language::JavaScript),
        ] {
            let out = SymbolExtractor::extract(Path::new(path), src, lang).unwrap();
            let imports = names(&out, SymbolKind::Import);
            assert_eq!(
                imports,
                vec![
                    "export * from './a';",
                    "export { b } from './b';",
                    "export * as ns from './c';"
                ],
                "{path}"
            );
        }
    }

    #[test]
    fn extract_javascript_non_require_call_is_not_import() {
        let out = SymbolExtractor::extract(
            Path::new("a.js"),
            "console.log('x');\nfoo();\n",
            Language::JavaScript,
        )
        .unwrap();
        assert!(names(&out, SymbolKind::Import).is_empty());
    }

    #[test]
    fn extract_ruby_symbols() {
        let src = "require 'json'\nrequire_relative 'x'\nputs 'hi'\nmodule M\n  class C\n    def run; end\n    def self.build; end\n  end\nend\n";
        let out = SymbolExtractor::extract(Path::new("a.rb"), src, Language::Ruby).unwrap();
        assert_eq!(names(&out, SymbolKind::Struct), vec!["M"]);
        assert_eq!(names(&out, SymbolKind::Class), vec!["C"]);
        assert_eq!(names(&out, SymbolKind::Function), vec!["run", "build"]);
        assert_eq!(names(&out, SymbolKind::Import).len(), 2);
    }

    #[test]
    fn extract_ruby_require_with_parens() {
        let out =
            SymbolExtractor::extract(Path::new("a.rb"), "require('a')\n", Language::Ruby).unwrap();
        assert_eq!(names(&out, SymbolKind::Import), vec!["require('a')"]);
    }

    #[test]
    fn extract_ruby_receiver_require_is_not_import() {
        let src = "Bundler.require(:default)\nKernel.require 'x'\nobj.require_relative 'y'\n";
        let out = SymbolExtractor::extract(Path::new("a.rb"), src, Language::Ruby).unwrap();
        assert!(names(&out, SymbolKind::Import).is_empty());
    }

    #[test]
    fn extract_ruby_singleton_class_has_no_class_name() {
        let src = "class << self\n  def x; end\nend\n";
        let out = SymbolExtractor::extract(Path::new("a.rb"), src, Language::Ruby).unwrap();
        assert!(names(&out, SymbolKind::Class).is_empty());
        assert_eq!(names(&out, SymbolKind::Function), vec!["x"]);
    }

    #[test]
    fn extract_ruby_bare_require_does_not_panic() {
        let out = SymbolExtractor::extract(Path::new("a.rb"), "require\n", Language::Ruby);
        assert!(out.is_ok());
    }

    #[test]
    fn extract_tolerates_syntax_errors() {
        let out =
            SymbolExtractor::extract(Path::new("a.rs"), "fn ok() {} fn (", Language::Rust).unwrap();
        assert!(names(&out, SymbolKind::Function).contains(&"ok"));
    }

    // ---- deep nesting (#417) ----

    /// Run extraction on a thread with a small stack: an unbounded recursive walk
    /// overflows it (SIGABRT) on deeply nested input, while a depth-capped walk does not.
    fn extract_small_stack(file: &str, src: String, lang: Language) -> Vec<(String, SymbolKind)> {
        let file = file.to_owned();
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || SymbolExtractor::extract(Path::new(&file), &src, lang).unwrap())
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn extract_deeply_nested_source_does_not_overflow_stack() {
        const N: usize = 30_000;
        let nest = |open: &str, close: &str| format!("{}1{}", open.repeat(N), close.repeat(N));
        let cases = [
            (
                "a.py",
                Language::Python,
                format!("def top():\n    pass\nx = {}\n", nest("[", "]")),
            ),
            (
                "a.js",
                Language::JavaScript,
                format!("function top() {{}}\nx = {};\n", nest("[", "]")),
            ),
            (
                "a.ts",
                Language::TypeScript,
                format!("function top() {{}}\nx = {};\n", nest("[", "]")),
            ),
            (
                "a.rs",
                Language::Rust,
                format!(
                    "fn top() {{}}\nfn deep() {{ let x = {}; }}\n",
                    nest("(", ")")
                ),
            ),
            (
                "a.go",
                Language::Go,
                format!("package p\nfunc top() {{}}\nvar x = {}\n", nest("(", ")")),
            ),
            (
                "a.rb",
                Language::Ruby,
                format!("def top; end\nx = {}\n", nest("[", "]")),
            ),
        ];
        for (file, lang, src) in cases {
            let syms = extract_small_stack(file, src, lang);
            assert!(
                names(&syms, SymbolKind::Function).contains(&"top"),
                "{file}: shallow symbol before the deep nesting must still be extracted"
            );
        }
    }
}
