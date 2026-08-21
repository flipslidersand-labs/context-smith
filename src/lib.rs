pub mod budget;
pub mod bundle_writer;
pub mod dep_builder;
pub mod embed;
pub mod index_builder;
pub mod search_index;
pub mod tokenizer;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    Go,
    TypeScript,
    JavaScript,
    Ruby,
    Unknown,
}

impl Language {
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Language::Rust,
            Some("py") => Language::Python,
            Some("go") => Language::Go,
            Some("ts" | "tsx" | "mts" | "cts") => Language::TypeScript,
            Some("js" | "jsx" | "mjs" | "cjs") => Language::JavaScript,
            Some("rb") => Language::Ruby,
            _ => Language::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::Go => "go",
            Language::TypeScript => "typescript",
            Language::JavaScript => "javascript",
            Language::Ruby => "ruby",
            Language::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Class,
    Impl,
    Import,
}

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Struct => "struct",
            SymbolKind::Class => "class",
            SymbolKind::Impl => "impl",
            SymbolKind::Import => "import",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub file_id: i64,
    pub name: String,
    pub kind: SymbolKind,
    pub line: u32,
    pub snippet: String,
}

pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let root = path.as_ref().canonicalize()?;
        Ok(GitRepo { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Unified diff of the last n_commits commits (empty string if repo has no history).
    pub fn recent_diff(&self, n_commits: usize) -> Result<String> {
        let repo = git2::Repository::open(&self.root)
            .with_context(|| format!("Failed to open git repo at {:?}", self.root))?;
        let head_commit = repo.head()?.peel_to_commit()?;

        let mut ancestor = head_commit.clone();
        for _ in 0..n_commits {
            if ancestor.parent_count() == 0 {
                break;
            }
            ancestor = ancestor.parent(0)?;
        }

        let head_tree = head_commit.tree()?;
        let anc_tree = ancestor.tree()?;
        let diff = repo.diff_tree_to_tree(Some(&anc_tree), Some(&head_tree), None)?;

        let mut patch = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            if let Ok(s) = std::str::from_utf8(line.content()) {
                patch.push_str(s);
            }
            true
        })?;
        Ok(patch)
    }

    /// Walk all tracked files in HEAD, returning (relative_path, Language, blob_sha, size).
    pub fn scan(&self) -> Result<Vec<(PathBuf, Language, String, i64)>> {
        let repo = git2::Repository::open(&self.root)
            .with_context(|| format!("Failed to open git repo at {:?}", self.root))?;
        let head = repo.head()?.peel_to_tree()?;
        let mut results = Vec::new();
        head.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                let name = entry.name().unwrap_or("");
                let rel = if root.is_empty() {
                    PathBuf::from(name)
                } else {
                    PathBuf::from(root).join(name)
                };
                let lang = Language::from_path(&rel);
                let sha = entry.id().to_string();
                let size = entry
                    .to_object(&repo)
                    .and_then(|o| o.peel_to_blob())
                    .map(|b| b.size() as i64)
                    .unwrap_or(0);
                results.push((rel, lang, sha, size));
            }
            git2::TreeWalkResult::Ok
        })?;
        Ok(results)
    }
}

pub struct SymbolExtractor;

impl SymbolExtractor {
    pub fn extract(
        path: &Path,
        source: &str,
        lang: Language,
    ) -> Result<Vec<(String, SymbolKind, u32)>> {
        match lang {
            Language::Rust => Self::extract_rust(path, source),
            Language::Python => Self::extract_python(path, source),
            Language::Go => Self::extract_go(path, source),
            Language::TypeScript => Self::extract_typescript(path, source),
            Language::JavaScript => Self::extract_javascript(path, source),
            Language::Ruby => Self::extract_ruby(path, source),
            Language::Unknown => Ok(vec![]),
        }
    }

    fn extract_rust(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .with_context(|| format!("Failed to load Rust grammar for {:?}", path))?;
        let tree = parser.parse(source, None).context("Rust parse failed")?;
        let root = tree.root_node();
        Ok(collect_rust_symbols(root, source.as_bytes()))
    }

    fn extract_python(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .with_context(|| format!("Failed to load Python grammar for {:?}", path))?;
        let tree = parser.parse(source, None).context("Python parse failed")?;
        let root = tree.root_node();
        Ok(collect_python_symbols(root, source.as_bytes()))
    }

    fn extract_go(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .with_context(|| format!("Failed to load Go grammar for {:?}", path))?;
        let tree = parser.parse(source, None).context("Go parse failed")?;
        let root = tree.root_node();
        Ok(collect_go_symbols(root, source.as_bytes()))
    }

    fn extract_typescript(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        // The TSX grammar is a superset of TS, so it parses both .ts and .tsx.
        parser
            .set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
            .with_context(|| format!("Failed to load TypeScript grammar for {:?}", path))?;
        let tree = parser
            .parse(source, None)
            .context("TypeScript parse failed")?;
        let root = tree.root_node();
        Ok(collect_ecma_symbols(root, source.as_bytes()))
    }

    fn extract_javascript(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .with_context(|| format!("Failed to load JavaScript grammar for {:?}", path))?;
        let tree = parser
            .parse(source, None)
            .context("JavaScript parse failed")?;
        let root = tree.root_node();
        Ok(collect_ecma_symbols(root, source.as_bytes()))
    }

    fn extract_ruby(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_ruby::LANGUAGE.into())
            .with_context(|| format!("Failed to load Ruby grammar for {:?}", path))?;
        let tree = parser.parse(source, None).context("Ruby parse failed")?;
        let root = tree.root_node();
        Ok(collect_ruby_symbols(root, source.as_bytes()))
    }
}

fn node_text<'a>(node: tree_sitter::Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.byte_range()]).unwrap_or("")
}

fn collect_rust_symbols(root: tree_sitter::Node, src: &[u8]) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    traverse_rust(root, src, &mut out);
    out
}

fn traverse_rust(node: tree_sitter::Node, src: &[u8], out: &mut Vec<(String, SymbolKind, u32)>) {
    let line = node.start_position().row as u32 + 1;
    match node.kind() {
        "function_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function, line));
            }
        }
        "struct_item" | "enum_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct, line));
            }
        }
        "trait_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class, line));
            }
        }
        "type_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct, line));
            }
        }
        "impl_item" => {
            if let Some(ty) = node.child_by_field_name("type") {
                out.push((node_text(ty, src).to_owned(), SymbolKind::Impl, line));
            }
        }
        "use_declaration" => {
            out.push((
                node_text(node, src)
                    .trim_end_matches(';')
                    .trim_start_matches("use ")
                    .to_owned(),
                SymbolKind::Import,
                line,
            ));
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            traverse_rust(child, src, out);
        }
    }
}

fn collect_python_symbols(root: tree_sitter::Node, src: &[u8]) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    collect_python_node(root, src, &mut out);
    out
}

fn collect_python_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
) {
    let line = node.start_position().row as u32 + 1;
    match node.kind() {
        "function_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function, line));
            }
        }
        "class_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class, line));
            }
        }
        "import_statement" | "import_from_statement" => {
            out.push((node_text(node, src).to_owned(), SymbolKind::Import, line));
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_python_node(child, src, out);
        }
    }
}

/// Symbol collector shared by TypeScript and JavaScript (TS is a superset of the
/// JS node kinds; unknown kinds are simply ignored).
fn collect_ecma_symbols(root: tree_sitter::Node, src: &[u8]) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    collect_ecma_node(root, src, &mut out);
    out
}

fn collect_ecma_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
) {
    let line = node.start_position().row as u32 + 1;
    match node.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "function_signature"
        | "method_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function, line));
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class, line));
            }
        }
        // TypeScript-only type declarations map to Struct.
        "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct, line));
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
                    out.push((node_text(name, src).to_owned(), SymbolKind::Function, line));
                }
            }
        }
        "import_statement" => {
            out.push((node_text(node, src).to_owned(), SymbolKind::Import, line));
        }
        // CommonJS `require('./x')` — recorded as an import for dependency resolution.
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function") {
                if node_text(func, src) == "require" {
                    out.push((node_text(node, src).to_owned(), SymbolKind::Import, line));
                }
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_ecma_node(child, src, out);
        }
    }
}

fn collect_go_symbols(root: tree_sitter::Node, src: &[u8]) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    collect_go_node(root, src, &mut out);
    out
}

fn collect_go_node(node: tree_sitter::Node, src: &[u8], out: &mut Vec<(String, SymbolKind, u32)>) {
    let line = node.start_position().row as u32 + 1;
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function, line));
            }
        }
        "type_declaration" => {
            for i in 0..node.child_count() {
                if let Some(spec) = node.child(i) {
                    if spec.kind() == "type_spec" {
                        if let Some(name) = spec.child_by_field_name("name") {
                            out.push((node_text(name, src).to_owned(), SymbolKind::Struct, line));
                        }
                    }
                }
            }
        }
        "import_declaration" => {
            out.push((node_text(node, src).to_owned(), SymbolKind::Import, line));
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_go_node(child, src, out);
        }
    }
}

fn collect_ruby_symbols(root: tree_sitter::Node, src: &[u8]) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    collect_ruby_node(root, src, &mut out);
    out
}

fn collect_ruby_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
) {
    let line = node.start_position().row as u32 + 1;
    match node.kind() {
        "method" | "singleton_method" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function, line));
            }
        }
        "class" | "singleton_class" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Class, line));
            }
        }
        "module" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Struct, line));
            }
        }
        // require / require_relative calls
        "call" => {
            if let Some(method) = node.child_by_field_name("method") {
                let m = node_text(method, src);
                if m == "require" || m == "require_relative" {
                    out.push((node_text(node, src).to_owned(), SymbolKind::Import, line));
                }
            }
        }
        _ => {}
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_ruby_node(child, src, out);
        }
    }
}
