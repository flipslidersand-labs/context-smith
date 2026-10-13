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
        let path = path.as_ref();
        path.canonicalize().with_context(|| {
            format!(
                "--repo '{}' does not exist or is not accessible",
                path.display()
            )
        })?;
        let repo = git2::Repository::open(path)
            .with_context(|| format!("'{}' is not a git repository", path.display()))?;
        // Prefer repo.workdir() so the stored root matches git's own logical
        // working directory, avoiding divergence when the path contains symlinks
        // (e.g. network mounts where canonicalize may fail or resolve differently).
        let root = repo
            .workdir()
            .with_context(|| {
                format!(
                    "'{}' is a bare repository (no working directory)",
                    path.display()
                )
            })?
            .to_path_buf();
        Ok(GitRepo { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Unified diff of the last n_commits commits (empty string if repo has no history).
    ///
    /// The accumulated patch is capped at `DIFF_MAX_BYTES` (512 KiB). Any content
    /// beyond that limit is dropped and a `[diff truncated]` marker is appended so
    /// callers always receive a valid (possibly incomplete) UTF-8 string that fits
    /// within the token budget.
    ///
    /// When the ancestor is the root commit (no parents) the diff is computed
    /// against the empty tree so the initial commit's files are included.
    pub fn recent_diff(&self, n_commits: usize) -> Result<String> {
        const DIFF_MAX_BYTES: usize = 512 * 1024; // 512 KiB

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
        // When ancestor == head (root commit with no parents after the loop),
        // diff against the empty tree so the initial files are visible.
        let anc_tree_opt = if ancestor.id() != head_commit.id() {
            Some(ancestor.tree()?)
        } else if ancestor.parent_count() == 0 {
            None // empty tree → shows full initial commit
        } else {
            Some(ancestor.tree()?)
        };
        let diff = repo.diff_tree_to_tree(anc_tree_opt.as_ref(), Some(&head_tree), None)?;

        let mut patch = String::new();
        let mut truncated = false;
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            if truncated {
                return true;
            }
            if let Ok(s) = std::str::from_utf8(line.content()) {
                if patch.len() + s.len() > DIFF_MAX_BYTES {
                    truncated = true;
                } else {
                    patch.push_str(s);
                }
            }
            true
        })?;
        if truncated {
            patch.push_str("\n[diff truncated]\n");
        }
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

/// Shared helpers for unit tests that need a real git repository.
#[cfg(test)]
pub(crate) mod test_util {
    use std::path::Path;

    /// Write `add` files, delete `remove` files, and commit the result on HEAD.
    /// Initialises the repository on first use.
    pub fn commit_files(dir: &Path, add: &[(&str, &str)], remove: &[&str]) {
        let repo = git2::Repository::open(dir)
            .or_else(|_| git2::Repository::init(dir))
            .unwrap();
        let mut index = repo.index().unwrap();
        for (rel, content) in add {
            let abs = dir.join(rel);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&abs, content).unwrap();
            index.add_path(Path::new(rel)).unwrap();
        }
        for rel in remove {
            std::fs::remove_file(dir.join(rel)).unwrap();
            index.remove_path(Path::new(rel)).unwrap();
        }
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), &sig, &sig, "test commit", &tree, &parents)
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::commit_files;
    use super::*;
    use tempfile::TempDir;

    fn names(syms: &[(String, SymbolKind, u32)], kind: SymbolKind) -> Vec<&str> {
        syms.iter()
            .filter(|(_, k, _)| *k == kind)
            .map(|(n, _, _)| n.as_str())
            .collect()
    }

    // ---- Language ----

    #[test]
    fn language_from_path_maps_known_extensions() {
        let cases = [
            ("a.rs", Language::Rust),
            ("a.py", Language::Python),
            ("a.go", Language::Go),
            ("a.ts", Language::TypeScript),
            ("a.tsx", Language::TypeScript),
            ("a.mts", Language::TypeScript),
            ("a.cts", Language::TypeScript),
            ("a.js", Language::JavaScript),
            ("a.jsx", Language::JavaScript),
            ("a.mjs", Language::JavaScript),
            ("a.cjs", Language::JavaScript),
            ("a.rb", Language::Ruby),
        ];
        for (p, expected) in cases {
            assert_eq!(Language::from_path(Path::new(p)), expected, "{p}");
        }
    }

    #[test]
    fn language_from_path_unknown_and_no_extension() {
        assert_eq!(Language::from_path(Path::new("README")), Language::Unknown);
        assert_eq!(Language::from_path(Path::new("a.txt")), Language::Unknown);
        assert_eq!(Language::from_path(Path::new("")), Language::Unknown);
        // Extension matching is case-sensitive.
        assert_eq!(Language::from_path(Path::new("A.RS")), Language::Unknown);
    }

    #[test]
    fn language_as_str_values() {
        assert_eq!(Language::Rust.as_str(), "rust");
        assert_eq!(Language::Python.as_str(), "python");
        assert_eq!(Language::Go.as_str(), "go");
        assert_eq!(Language::TypeScript.as_str(), "typescript");
        assert_eq!(Language::JavaScript.as_str(), "javascript");
        assert_eq!(Language::Ruby.as_str(), "ruby");
        assert_eq!(Language::Unknown.as_str(), "unknown");
    }

    #[test]
    fn symbol_kind_as_str_values() {
        assert_eq!(SymbolKind::Function.as_str(), "function");
        assert_eq!(SymbolKind::Struct.as_str(), "struct");
        assert_eq!(SymbolKind::Class.as_str(), "class");
        assert_eq!(SymbolKind::Impl.as_str(), "impl");
        assert_eq!(SymbolKind::Import.as_str(), "import");
    }

    // ---- GitRepo::new ----

    #[test]
    fn gitrepo_new_nonexistent_path_errors() {
        let tmp = TempDir::new().unwrap();
        let err = GitRepo::new(tmp.path().join("missing")).err().unwrap();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn gitrepo_new_non_git_dir_errors() {
        let tmp = TempDir::new().unwrap();
        let err = GitRepo::new(tmp.path()).err().unwrap();
        assert!(err.to_string().contains("not a git repository"), "{err}");
    }

    #[test]
    fn gitrepo_new_bare_repo_errors() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init_bare(tmp.path()).unwrap();
        let err = GitRepo::new(tmp.path()).err().unwrap();
        assert!(err.to_string().contains("bare repository"), "{err}");
    }

    #[test]
    fn gitrepo_new_valid_repo_exposes_root() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(
            repo.root().canonicalize().unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    // ---- GitRepo::scan ----

    #[test]
    fn scan_returns_nested_files_with_lang_and_size() {
        let tmp = TempDir::new().unwrap();
        commit_files(
            tmp.path(),
            &[("src/main.rs", "fn main() {}\n"), ("notes.txt", "hello")],
            &[],
        );
        let repo = GitRepo::new(tmp.path()).unwrap();
        let mut files = repo.scan().unwrap();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, PathBuf::from("notes.txt"));
        assert_eq!(files[0].1, Language::Unknown);
        assert_eq!(files[0].3, 5);
        assert_eq!(files[1].0, PathBuf::from("src/main.rs"));
        assert_eq!(files[1].1, Language::Rust);
        assert_eq!(files[1].3, 13);
        assert_eq!(files[1].2.len(), 40, "blob sha should be 40 hex chars");
    }

    #[test]
    fn scan_without_commits_errors() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert!(repo.scan().is_err());
    }

    // ---- GitRepo::recent_diff ----

    #[test]
    // Note: DiffFormat::Patch callback content() excludes the +/- origin char.
    fn recent_diff_root_commit_shows_initial_files() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.txt", "first\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("first"), "{diff}");
    }

    #[test]
    fn recent_diff_covers_last_n_commits_only() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.txt", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.txt", "two\n")], &[]);
        commit_files(tmp.path(), &[("c.txt", "three\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("three"));
        assert!(!diff.contains("two"));
        let diff2 = repo.recent_diff(2).unwrap();
        assert!(diff2.contains("three") && diff2.contains("two"));
        assert!(!diff2.contains("one"));
    }

    #[test]
    fn recent_diff_n_larger_than_history_clamps_to_root() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.txt", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.txt", "two\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(100).unwrap();
        assert!(diff.contains("two"), "{diff}");
    }

    #[test]
    fn recent_diff_zero_commits_is_empty() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.txt", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.txt", "two\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(repo.recent_diff(0).unwrap(), "");
    }

    #[test]
    fn recent_diff_without_commits_errors() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert!(repo.recent_diff(1).is_err());
    }

    #[test]
    fn recent_diff_truncates_large_patch_with_marker() {
        let tmp = TempDir::new().unwrap();
        let big = "0123456789abcdef\n".repeat(40_000); // ~680 KB > 512 KiB cap
        commit_files(tmp.path(), &[("big.txt", big.as_str())], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.ends_with("[diff truncated]\n"));
        assert!(diff.len() <= 512 * 1024 + "\n[diff truncated]\n".len());
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
        // `use ` prefix and trailing `;` are trimmed; line numbers are 1-based.
        assert_eq!(out[0], ("std::fmt".to_string(), SymbolKind::Import, 1));
        assert!(out.contains(&("run".to_string(), SymbolKind::Function, 7)));
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
    fn extract_go_symbols() {
        let src = "package main\n\nimport \"fmt\"\n\ntype Server struct{}\n\nfunc (s *Server) Run() {}\n\nfunc main() { fmt.Println() }\n";
        let out = SymbolExtractor::extract(Path::new("a.go"), src, Language::Go).unwrap();
        assert_eq!(names(&out, SymbolKind::Struct), vec!["Server"]);
        assert_eq!(names(&out, SymbolKind::Function), vec!["Run", "main"]);
        assert_eq!(names(&out, SymbolKind::Import).len(), 1);
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
    fn extract_tolerates_syntax_errors() {
        let out =
            SymbolExtractor::extract(Path::new("a.rs"), "fn ok() {} fn (", Language::Rust).unwrap();
        assert!(names(&out, SymbolKind::Function).contains(&"ok"));
    }
}
