mod budget;
mod bundle_writer;
mod dep_builder;
#[cfg_attr(not(feature = "remote-embed"), allow(dead_code))]
mod embed;
mod index_builder;
mod path_kind;
mod search_index;
mod select;
mod tokenizer;

pub use budget::Candidate;
pub use bundle_writer::{render_task_md, write_bundle};
#[cfg(feature = "remote-embed")]
pub use embed::embed_index;
pub use index_builder::{build_index, IndexDb, IndexStats};
pub use select::{select_candidates, SeedMode, SelectError, SelectOptions, Selection};

/// True if the commit's raw object records a parent, even when libgit2 hides it
/// (shallow boundary). A true root has no `parent ` header line.
fn has_raw_parent(repo: &git2::Repository, commit: &git2::Commit) -> Result<bool> {
    let odb = repo.odb()?;
    let obj = odb.read(commit.id())?;
    let header = obj
        .data()
        .split(|&b| b == b'\n')
        .take_while(|l| !l.is_empty());
    Ok(header.into_iter().any(|l| l.starts_with(b"parent ")))
}

#[cfg(test)]
#[path = "tests/mod.rs"]
mod moved;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Language {
    Rust,
    Python,
    Go,
    TypeScript,
    JavaScript,
    Ruby,
    Unknown,
}

impl Language {
    pub(crate) fn from_path(path: &Path) -> Self {
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

    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) enum SymbolKind {
    Function,
    Struct,
    Class,
    Impl,
    Import,
}

impl SymbolKind {
    pub(crate) fn as_str(&self) -> &'static str {
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
pub(crate) struct Symbol {
    pub file_id: i64,
    pub name: String,
    pub kind: SymbolKind,
    pub line: u32,
    pub snippet: String,
}

#[non_exhaustive]
pub struct GitRepo {
    root: PathBuf,
    /// Canonicalized `root`, computed once; used to validate file reads.
    canonical_root: PathBuf,
}

/// Marker payload of the `io::Error` returned when a read is refused because
/// the resolved target is outside the repo, inside a `.git` directory, or an
/// untracked file (symlink target, or a path absent from the git index).
#[derive(Debug)]
pub(crate) struct PathEscape;

impl std::fmt::Display for PathEscape {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("path resolves outside the repository, into .git, or to an untracked file")
    }
}

impl std::error::Error for PathEscape {}

/// True if `e` is a [`PathEscape`] refusal (as opposed to a genuine I/O error
/// such as EACCES).
pub(crate) fn is_path_escape(e: &std::io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<PathEscape>())
}

/// Read `path` as UTF-8 text only if its fully resolved target lies inside
/// `canonical_root` and has no `.git` path component at any depth (#202).
/// Refusals carry a [`PathEscape`] payload. Symlinks, `..` and symlinked
/// directories are all resolved by canonicalize immediately before the read.
pub(crate) fn read_within_root(canonical_root: &Path, path: &Path) -> std::io::Result<String> {
    let bytes = read_bytes_within_root(canonical_root, path)?;
    String::from_utf8(bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// True if `real` (canonical, inside `canonical_root`) is a stage-0 entry in the
/// repo's index. Fails closed on any error.
fn is_tracked_in_index(canonical_root: &Path, real: &Path) -> bool {
    let Ok(rel) = real.strip_prefix(canonical_root) else {
        return false;
    };
    let Some(rel) = rel.to_str() else {
        return false;
    };
    git2::Repository::open(canonical_root)
        .and_then(|r| r.index())
        .map(|idx| idx.get_path(Path::new(rel), 0).is_some())
        .unwrap_or(false)
}

/// True if `path` or any ancestor below the repo root is a symlink. Walks up,
/// checking each component itself *before* deciding whether it is the root, so a
/// link that resolves to the root (`docs -> .`) is still caught. Fails closed
/// (true) if the root is never reached.
fn path_has_symlink_component(canonical_root: &Path, path: &Path) -> bool {
    for (i, anc) in path.ancestors().enumerate() {
        match std::fs::symlink_metadata(anc) {
            Ok(m) if m.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return true,
        }
        if i > 0 && anc.canonicalize().is_ok_and(|p| p == canonical_root) {
            return false;
        }
    }
    true
}

pub(crate) fn read_bytes_within_root(
    canonical_root: &Path,
    path: &Path,
) -> std::io::Result<Vec<u8>> {
    let real = path.canonicalize()?;
    let denied = match real.strip_prefix(canonical_root) {
        Err(_) => true,
        Ok(rest) => rest.components().any(|c| c.as_os_str() == ".git"),
    };
    if denied {
        return Err(std::io::Error::other(PathEscape));
    }
    // A symlink may only lead to a *tracked* file; otherwise a committed link
    // (e.g. x.py -> .env) would expose untracked / gitignored files (#328).
    // Any path component (not just the last) may be a symlink, e.g. a tracked
    // `docs -> ignored_dir` directory link.
    let is_link = path_has_symlink_component(canonical_root, path);
    if is_link && !is_tracked_in_index(canonical_root, &real) {
        return Err(std::io::Error::other(PathEscape));
    }
    // Never open FIFOs / devices / sockets: open(2)/read would block (#494).
    if !std::fs::metadata(&real)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    std::fs::read(real)
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
        let canonical_root = root
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize repo root {:?}", root))?;
        Ok(GitRepo {
            root,
            canonical_root,
        })
    }

    /// Validated read of a tracked file (relative to the repo root). Refuses
    /// targets outside the repo or under `.git`, regardless of file mode.
    pub(crate) fn read_tracked(&self, rel: &Path) -> std::io::Result<String> {
        read_within_root(&self.canonical_root, &self.root.join(rel))
    }

    /// Open the repo's git index once so callers can check many paths with
    /// [`GitRepo::read_indexed`] without reopening it per file.
    pub(crate) fn open_index(&self) -> Result<git2::Index> {
        git2::Repository::open(&self.root)
            .and_then(|r| r.index())
            .with_context(|| format!("Failed to read git index at {:?}", self.root))
    }

    /// Like [`GitRepo::read_tracked`], but additionally refuses a path that is
    /// not a stage-0 entry in `index`, so a path taken from an untrusted
    /// index.db cannot expose untracked files such as `.env` (#395). Uncommitted
    /// edits to tracked files are still readable.
    pub(crate) fn read_indexed(&self, index: &git2::Index, rel: &Path) -> std::io::Result<String> {
        if index.get_path(rel, 0).is_none() {
            return Err(std::io::Error::other(PathEscape));
        }
        self.read_tracked(rel)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The canonicalized repository root (symlinks resolved).
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Unified diff of the last n_commits commits (empty string if repo has no history).
    ///
    /// The accumulated patch is capped at `DIFF_MAX_BYTES` (512 KiB). Any content
    /// beyond that limit is dropped and a `[diff truncated]` marker is appended so
    /// callers always receive a valid (possibly incomplete) UTF-8 string that fits
    /// within the token budget.
    ///
    /// When `n_commits` reaches past the root commit the diff is computed
    /// against the empty tree so the initial commit's files are included. In a
    /// shallow clone the oldest reachable commit is not a real root, so the diff
    /// is taken against that commit's tree instead (older history is unavailable).
    /// Non-UTF-8 lines are converted lossily (U+FFFD), not dropped.
    pub fn recent_diff(&self, n_commits: usize) -> Result<String> {
        const DIFF_MAX_BYTES: usize = 512 * 1024; // 512 KiB

        let repo = git2::Repository::open(&self.root)
            .with_context(|| format!("Failed to open git repo at {:?}", self.root))?;
        let head_commit = repo.head()?.peel_to_commit()?;

        // 0 means "no diff". Without this, a root-only history would fall into the
        // empty-tree branch below and return the whole initial commit.
        if n_commits == 0 {
            return Ok(String::new());
        }

        // Walk back n commits. If history runs out first (the root commit would
        // need a parent), diff against the empty tree so the root's files count.
        let mut ancestor = head_commit.clone();
        let mut anc_tree_opt = None;
        let mut reached = true;
        for _ in 0..n_commits {
            if ancestor.parent_count() == 0 {
                reached = false;
                break;
            }
            ancestor = ancestor.parent(0)?;
        }
        let head_tree = head_commit.tree()?;
        // A shallow clone's boundary commit has no parent in the object store but
        // is not the true root: diff against its tree (the oldest commit we can
        // see) instead of the empty tree, or the whole repo shows up (#465).
        // `is_shallow()` is repo-wide: a shallow second-parent side must not turn a
        // true first-parent root into a boundary, so inspect the commit itself (#494).
        let at_boundary = !reached && repo.is_shallow() && has_raw_parent(&repo, &ancestor)?;
        if reached || at_boundary {
            anc_tree_opt = Some(ancestor.tree()?);
            if !reached {
                eprintln!(
                    "note: shallow clone: diffing against the oldest available commit {} (older history is unavailable; the diff may be partial or empty)",
                    ancestor.id()
                );
            }
        }
        let diff = repo.diff_tree_to_tree(anc_tree_opt.as_ref(), Some(&head_tree), None)?;

        // Only files still tracked in the index with a known language are
        // shown. A deleted file (e.g. a removed `.env`) is not in the index, so
        // its `-` lines cannot leak a secret into the bundle (#447).
        let index = repo.index()?;
        let mut cached: Option<(Vec<u8>, bool)> = None;
        let mut patch = String::new();
        let mut truncated = false;
        diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
            if truncated {
                return true;
            }
            let allowed = match delta.new_file().path() {
                Some(path) if delta.status() != git2::Delta::Deleted => {
                    let key = path.as_os_str().as_encoded_bytes();
                    match &cached {
                        Some((k, ok)) if k == key => *ok,
                        _ => {
                            let ok = Language::from_path(path) != Language::Unknown
                                && index.get_path(path, 0).is_some();
                            cached = Some((key.to_vec(), ok));
                            ok
                        }
                    }
                }
                _ => false,
            };
            if !allowed {
                return true;
            }
            // Invalid UTF-8 bytes become U+FFFD instead of dropping the line.
            let s = String::from_utf8_lossy(line.content());
            let s = s.as_ref();
            // content() omits the +/-/space origin char for body lines; header
            // ('F'/'H'), binary ('B') and no-newline ('=', '>', '<') lines
            // already carry their full text.
            let prefix = match line.origin() {
                o @ ('+' | '-' | ' ') => Some(o),
                _ => None,
            };
            let extra = prefix.map_or(0, char::len_utf8);
            if patch.len() + extra + s.len() > DIFF_MAX_BYTES {
                truncated = true;
            } else {
                patch.extend(prefix);
                patch.push_str(s);
            }
            true
        })?;
        if truncated {
            patch.push_str("\n[diff truncated]\n");
        }
        Ok(patch)
    }

    /// Walk all tracked files in HEAD, returning (relative_path, Language, blob_sha, size).
    ///
    /// The sha and size describe the working-tree content (git blob hash), so
    /// uncommitted edits are noticed and the sha equals HEAD's for an unchanged
    /// file. Unknown-language files keep HEAD's values; a file missing or
    /// unreadable in the working tree gets an `unreadable:`-prefixed sha so it is
    /// never treated as unchanged.
    #[cfg(test)]
    pub(crate) fn scan(&self) -> Result<Vec<(PathBuf, Language, String, i64)>> {
        Ok(self.scan_with_skipped()?.0)
    }

    /// Like [`scan`](Self::scan), but also returns how many tracked files were
    /// skipped because their path is not valid UTF-8 (each one is warned
    /// about individually; the scan continues, #308).
    pub(crate) fn scan_with_skipped(&self) -> Result<(Vec<ScanEntry>, usize)> {
        let repo = git2::Repository::open(&self.root)
            .with_context(|| format!("Failed to open git repo at {:?}", self.root))?;
        let head = repo.head()?.peel_to_tree()?;
        let mut results = Vec::new();
        let mut skipped = 0usize;
        self.scan_tree(&repo, &head, Vec::new(), &mut results, &mut skipped)?;
        Ok((results, skipped))
    }

    /// Pre-order walk keeping the path as raw bytes so a non-UTF-8 name only
    /// affects its own entries instead of aborting the whole walk.
    fn scan_tree(
        &self,
        repo: &git2::Repository,
        tree: &git2::Tree<'_>,
        prefix: Vec<u8>,
        results: &mut Vec<(PathBuf, Language, String, i64)>,
        skipped: &mut usize,
    ) -> Result<()> {
        for entry in tree.iter() {
            let mut full = prefix.clone();
            full.extend_from_slice(entry.name_bytes());
            match entry.kind() {
                Some(git2::ObjectType::Tree) => {
                    full.push(b'/');
                    let sub = entry.to_object(repo)?.peel_to_tree()?;
                    self.scan_tree(repo, &sub, full, results, skipped)?;
                }
                Some(git2::ObjectType::Blob) => {
                    let Ok(name) = String::from_utf8(full) else {
                        eprintln!(
                            "warn: skip non-UTF-8 path in {:?}",
                            String::from_utf8_lossy(&prefix)
                        );
                        *skipped += 1;
                        continue;
                    };
                    let rel = PathBuf::from(name);
                    let lang = Language::from_path(&rel);
                    let mut sha = entry.id().to_string();
                    // Header only: avoids loading every blob's content just for its size.
                    let mut size = repo
                        .odb()
                        .and_then(|odb| odb.read_header(entry.id()))
                        .map(|(size, _)| size as i64)
                        .unwrap_or(0);
                    if lang != Language::Unknown {
                        match read_bytes_within_root(&self.canonical_root, &self.root.join(&rel)) {
                            Ok(bytes) => {
                                if let Ok(oid) =
                                    git2::Oid::hash_object(git2::ObjectType::Blob, &bytes)
                                {
                                    sha = oid.to_string();
                                    size = bytes.len() as i64;
                                }
                            }
                            Err(e) if is_path_escape(&e) => {
                                eprintln!("warn: skip {:?}: {e}", rel);
                                continue;
                            }
                            // Missing/unreadable in the working tree: HEAD's sha
                            // would look "unchanged" and keep stale rows. A
                            // sentinel forces a re-read that fails and drops them.
                            Err(_) => sha = format!("unreadable:{sha}"),
                        }
                    }
                    results.push((rel, lang, sha, size));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// (relative_path, language, blob_sha, size) as returned by [`GitRepo::scan`].
pub(crate) type ScanEntry = (PathBuf, Language, String, i64);

pub(crate) struct SymbolExtractor;

impl SymbolExtractor {
    pub(crate) fn extract(
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
        Ok(collect_rust_symbols(root, source.as_bytes(), path))
    }

    fn extract_python(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .with_context(|| format!("Failed to load Python grammar for {:?}", path))?;
        let tree = parser.parse(source, None).context("Python parse failed")?;
        let root = tree.root_node();
        Ok(collect_python_symbols(root, source.as_bytes(), path))
    }

    fn extract_go(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .with_context(|| format!("Failed to load Go grammar for {:?}", path))?;
        let tree = parser.parse(source, None).context("Go parse failed")?;
        let root = tree.root_node();
        Ok(collect_go_symbols(root, source.as_bytes(), path))
    }

    fn extract_typescript(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        // TSX is not a superset of TS: `<T>x` assertions and `<T>(x) => x` parse as JSX
        // there. Use the TSX grammar only for `.tsx`; `.ts`/`.mts`/`.cts` use plain TS.
        let grammar = if path.extension().and_then(|e| e.to_str()) == Some("tsx") {
            tree_sitter_typescript::LANGUAGE_TSX
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT
        };
        parser
            .set_language(&grammar.into())
            .with_context(|| format!("Failed to load TypeScript grammar for {:?}", path))?;
        let tree = parser
            .parse(source, None)
            .context("TypeScript parse failed")?;
        let root = tree.root_node();
        Ok(collect_ecma_symbols(root, source.as_bytes(), path))
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
        Ok(collect_ecma_symbols(root, source.as_bytes(), path))
    }

    fn extract_ruby(path: &Path, source: &str) -> Result<Vec<(String, SymbolKind, u32)>> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_ruby::LANGUAGE.into())
            .with_context(|| format!("Failed to load Ruby grammar for {:?}", path))?;
        let tree = parser.parse(source, None).context("Ruby parse failed")?;
        let root = tree.root_node();
        Ok(collect_ruby_symbols(root, source.as_bytes(), path))
    }
}

/// Maximum syntax-tree depth the recursive symbol walkers descend. Deeper branches are
/// skipped so a pathologically nested file cannot overflow the stack (#417). Real code
/// nests well below this.
const MAX_WALK_DEPTH: usize = 500;

fn warn_if_truncated(path: &Path, truncated: bool) {
    if truncated {
        eprintln!(
            "warn: {}: nesting deeper than {MAX_WALK_DEPTH}, deeper nodes skipped",
            path.display()
        );
    }
}

fn node_text<'a>(node: tree_sitter::Node, src: &'a [u8]) -> &'a str {
    std::str::from_utf8(&src[node.byte_range()]).unwrap_or("")
}

fn collect_rust_symbols(
    root: tree_sitter::Node,
    src: &[u8],
    path: &Path,
) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    let mut truncated = false;
    traverse_rust(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn traverse_rust(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
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
            // Take the `argument` field so visibility modifiers (`pub`,
            // `pub(crate)`, `pub(in path)`) are excluded. A missing/empty argument
            // (syntax-error recovery such as `pub use ;`) is not recorded.
            if let Some(arg) = node.child_by_field_name("argument") {
                let text = node_text(arg, src).trim();
                if !text.is_empty() {
                    out.push((text.to_owned(), SymbolKind::Import, line));
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
                    out.push((
                        format!("mod {}", node_text(name, src)),
                        SymbolKind::Import,
                        line,
                    ));
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
) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_python_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_python_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
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
) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_ecma_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_ecma_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
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
        // Re-exports (`export ... from './x'`) are dependencies; plain `export { a }` is not.
        "export_statement" => {
            if node.child_by_field_name("source").is_some() {
                out.push((node_text(node, src).to_owned(), SymbolKind::Import, line));
            }
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
) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_go_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_go_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
    let line = node.start_position().row as u32 + 1;
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.push((node_text(name, src).to_owned(), SymbolKind::Function, line));
            }
        }
        "type_declaration" => {
            for i in 0..node.child_count() as u32 {
                if let Some(spec) = node.child(i) {
                    if matches!(spec.kind(), "type_spec" | "type_alias") {
                        if let Some(name) = spec.child_by_field_name("name") {
                            // Use the spec's own row so grouped `type ( ... )` entries point at their definition.
                            let spec_line = spec.start_position().row as u32 + 1;
                            out.push((
                                node_text(name, src).to_owned(),
                                SymbolKind::Struct,
                                spec_line,
                            ));
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
) -> Vec<(String, SymbolKind, u32)> {
    let mut out = Vec::new();
    let mut truncated = false;
    collect_ruby_node(root, src, &mut out, 0, &mut truncated);
    warn_if_truncated(path, truncated);
    out
}

fn collect_ruby_node(
    node: tree_sitter::Node,
    src: &[u8],
    out: &mut Vec<(String, SymbolKind, u32)>,
    depth: usize,
    truncated: &mut bool,
) {
    if depth > MAX_WALK_DEPTH {
        *truncated = true;
        return;
    }
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
                // Receiver-less only: `Bundler.require(:default)` / `Kernel.require 'x'` are not imports.
                if (m == "require" || m == "require_relative")
                    && node.child_by_field_name("receiver").is_none()
                {
                    out.push((node_text(node, src).to_owned(), SymbolKind::Import, line));
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

/// COMMIT the open transaction; if COMMIT fails, ROLLBACK so the connection is
/// not left inside a transaction (#231). The original COMMIT error is returned;
/// a failing ROLLBACK is ignored.
pub(crate) fn commit_or_rollback(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch("COMMIT").inspect_err(|_| {
        let _ = conn.execute_batch("ROLLBACK");
    })
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

    /// #494: a FIFO must be refused without blocking on open/read.
    #[cfg(unix)]
    #[test]
    fn read_bytes_within_root_skips_fifo_without_blocking() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let fifo = root.join("pipe.py");
        match std::process::Command::new("mkfifo").arg(&fifo).status() {
            Ok(st) if st.success() => {}
            _ => return, // mkfifo unavailable: skip
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let r = root.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_bytes_within_root(&r, &r.join("pipe.py")));
        });
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read blocked on FIFO");
        assert!(res.is_err());
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
    fn scan_size_matches_blob_size_for_text_and_binary() {
        let tmp = TempDir::new().unwrap();
        // Unknown-language entries keep HEAD's size, so this exercises the
        // header-only path directly.
        commit_files(
            tmp.path(),
            &[("data.bin", "\0\u{1}\0binary\0"), ("empty.dat", "")],
            &[],
        );
        let repo = GitRepo::new(tmp.path()).unwrap();
        let mut files = repo.scan().unwrap();
        files.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(files[0].0, PathBuf::from("data.bin"));
        assert_eq!(files[0].3, 10);
        assert_eq!(files[1].0, PathBuf::from("empty.dat"));
        assert_eq!(files[1].3, 0);
    }

    #[test]
    fn scan_without_commits_errors() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert!(repo.scan().is_err());
    }

    #[test]
    fn scan_ignores_gitlink_entries() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "fn a() {}\n")], &[]);
        // Add a gitlink (submodule, mode 160000) pointing at an arbitrary oid.
        let git = git2::Repository::open(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        let entry = git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: git2::Oid::from_str("1234567890123456789012345678901234567890").unwrap(),
            flags: 0,
            flags_extended: 0,
            path: b"sub".to_vec(),
        };
        index.add(&entry).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        assert!(
            tree.iter().any(|e| e.name_bytes() == b"sub"),
            "gitlink must be in HEAD tree"
        );
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "add gitlink", &tree, &[&parent])
            .unwrap();

        let repo = GitRepo::new(tmp.path()).unwrap();
        let names: Vec<String> = repo
            .scan()
            .unwrap()
            .into_iter()
            .map(|(p, ..)| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.rs".to_string()]);

        let db = crate::index_builder::IndexDb::open_in_memory().unwrap();
        let stats = crate::index_builder::build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_total, 1);
    }

    // ---- GitRepo::recent_diff ----

    #[test]
    // DiffFormat::Patch callback content() excludes the +/-/space origin char;
    // recent_diff re-adds it from line.origin().
    fn recent_diff_root_commit_shows_initial_files() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "first\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("\n+first\n"), "{diff}");
    }

    /// #447: a commit that deletes a secret must not leak its `-` lines, and
    /// files of unknown language are left out of the diff.
    #[test]
    fn recent_diff_omits_deleted_files_and_unknown_language() {
        let tmp = TempDir::new().unwrap();
        commit_files(
            tmp.path(),
            &[
                ("keep.rs", "fn a() {}\n"),
                ("secret.rs", "const K: &str = \"fake-removed-secret\";\n"),
            ],
            &[],
        );
        commit_files(
            tmp.path(),
            &[
                ("notes.txt", "fake-unknown-lang\n"),
                ("keep.rs", "fn b() {}\n"),
            ],
            &["secret.rs"],
        );
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("+fn b() {}"), "{diff}");
        assert!(!diff.contains("fake-removed-secret"), "{diff}");
        assert!(!diff.contains("secret.rs"), "{diff}");
        assert!(!diff.contains("fake-unknown-lang"), "{diff}");
    }

    #[test]
    fn recent_diff_prefixes_added_removed_and_context_lines() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "keep\nold\ntail\n")], &[]);
        commit_files(tmp.path(), &[("a.rs", "keep\nnew\ntail\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("\n-old\n"), "{diff}");
        assert!(diff.contains("\n+new\n"), "{diff}");
        assert!(diff.contains("\n keep\n"), "{diff}");
        assert!(diff.contains("\n tail\n"), "{diff}");
        // File/hunk headers are not double-prefixed.
        assert!(diff.starts_with("diff --git"), "{diff}");
        assert!(diff.contains("\n@@ "), "{diff}");
    }

    #[test]
    fn recent_diff_keeps_no_newline_marker_unprefixed() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "old")], &[]);
        commit_files(tmp.path(), &[("a.rs", "new")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(
            diff.contains("-old\n\\ No newline at end of file\n"),
            "{diff}"
        );
        assert!(
            diff.contains("+new\n\\ No newline at end of file\n"),
            "{diff}"
        );
    }

    #[test]
    fn recent_diff_cap_counts_origin_byte() {
        // Body lines are 3 bytes ("+x\n"). A first line of 0..=2 extra bytes
        // shifts where the cap lands relative to the line stride; for one of
        // the three shifts the patch stops exactly 2 bytes short of the cap,
        // where a check that ignored the origin byte would accept one more
        // line and overshoot the cap by 1.
        for pad in ["", "y", "yy"] {
            let tmp = TempDir::new().unwrap();
            let big = format!("{pad}\n{}", "x\n".repeat(300_000));
            commit_files(tmp.path(), &[("n.rs", big.as_str())], &[]);
            let repo = GitRepo::new(tmp.path()).unwrap();
            let diff = repo.recent_diff(1).unwrap();
            let body = diff.strip_suffix("\n[diff truncated]\n").expect("marker");
            assert!(body.len() <= 512 * 1024, "pad {pad:?}: {}", body.len());
        }
    }

    #[test]
    fn recent_diff_covers_last_n_commits_only() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        commit_files(tmp.path(), &[("c.rs", "three\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.contains("three"));
        assert!(!diff.contains("two"));
        let diff2 = repo.recent_diff(2).unwrap();
        assert!(diff2.contains("three") && diff2.contains("two"));
        assert!(!diff2.contains("one"));
    }

    /// #465: in a shallow clone the boundary commit is not the true root, so
    /// the diff must not fall back to the empty tree (whole repo).
    #[test]
    fn recent_diff_shallow_clone_does_not_include_whole_repo() {
        let src = TempDir::new().unwrap();
        for (f, c) in [
            ("a.rs", "one"),
            ("b.rs", "two"),
            ("c.rs", "three"),
            ("d.rs", "four"),
        ] {
            commit_files(src.path(), &[(f, &format!("{c}\n"))], &[]);
        }
        let dst = TempDir::new().unwrap();
        let clone = dst.path().join("clone");
        let url = format!("file://{}", src.path().display());
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth", "2", &url])
            .arg(&clone)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let repo = GitRepo::new(&clone).unwrap();
        let diff = repo.recent_diff(3).unwrap();
        assert!(diff.contains("+four"), "{diff}");
        assert!(!diff.contains("+one") && !diff.contains("+two"), "{diff}");
        assert!(!diff.contains("+three"), "{diff}");
    }

    /// Runs `git` in `dir` (test helper for the shallow-merge fixtures).
    fn git_cli(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// R -> M1 -> Merge(second parent: S1..S5 forked from R), cloned with depth 3.
    /// First-parent reaches the true root R while the second-parent side ends at a
    /// shallow boundary, so the repo is shallow but R is a real root.
    fn shallow_merge_clone(dst: &Path) -> std::path::PathBuf {
        let src = dst.join("src");
        std::fs::create_dir_all(&src).unwrap();
        git_cli(&src, &["init", "-q", "-b", "main"]);
        commit_files(&src, &[("root.rs", "root_line\n")], &[]);
        git_cli(&src, &["checkout", "-q", "-b", "side"]);
        for i in 1..=5 {
            commit_files(&src, &[(&format!("s{i}.rs"), "side\n")], &[]);
        }
        git_cli(&src, &["checkout", "-q", "main"]);
        commit_files(&src, &[("m1.rs", "m1_line\n")], &[]);
        git_cli(&src, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
        let clone = dst.join("clone");
        let url = format!("file://{}", src.display());
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth", "3", &url])
            .arg(&clone)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        clone
    }

    /// #494: a shallow second-parent side must not make a true first-parent root
    /// look like a shallow boundary (its files would drop out of the diff).
    #[test]
    fn recent_diff_true_root_in_shallow_merge_clone_includes_root_files() {
        let dst = TempDir::new().unwrap();
        let clone = shallow_merge_clone(dst.path());
        assert!(git2::Repository::open(&clone).unwrap().is_shallow());
        let diff = GitRepo::new(&clone).unwrap().recent_diff(3).unwrap();
        assert!(diff.contains("+root_line"), "{diff}");
    }

    /// #494: depth 1 makes HEAD itself the boundary; the empty diff is explained on stderr
    /// by the code path, and must still be empty rather than the whole repo.
    #[test]
    fn recent_diff_depth_one_clone_is_empty() {
        let src = TempDir::new().unwrap();
        commit_files(src.path(), &[("a.rs", "one\n")], &[]);
        commit_files(src.path(), &[("b.rs", "two\n")], &[]);
        let dst = TempDir::new().unwrap();
        let clone = dst.path().join("clone");
        let url = format!("file://{}", src.path().display());
        let out = std::process::Command::new("git")
            .args(["clone", "--quiet", "--depth", "1", &url])
            .arg(&clone)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let diff = GitRepo::new(&clone).unwrap().recent_diff(3).unwrap();
        assert!(!diff.contains("+one"), "{diff}");
    }

    #[test]
    fn recent_diff_n_larger_than_history_clamps_to_root() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(100).unwrap();
        assert!(diff.contains("two"), "{diff}");
        assert!(diff.contains("+one"), "{diff}");
    }

    #[test]
    fn recent_diff_n_larger_than_three_commit_history_includes_root_files() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        commit_files(tmp.path(), &[("c.rs", "three\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(5).unwrap();
        assert!(
            diff.contains("+one") && diff.contains("+two") && diff.contains("+three"),
            "{diff}"
        );
        // n = history length: one step past the root, so root files are included.
        let diff3 = repo.recent_diff(3).unwrap();
        assert!(diff3.contains("+one"), "{diff3}");
        // Exactly reaching the root (n = commits - 1) still excludes root files.
        let diff2 = repo.recent_diff(2).unwrap();
        assert!(!diff2.contains("+one"), "{diff2}");
    }

    #[test]
    fn recent_diff_single_commit_with_large_n_shows_files() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "first\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert!(repo.recent_diff(5).unwrap().contains("+first"));
    }

    #[test]
    fn recent_diff_zero_commits_is_empty() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "one\n")], &[]);
        commit_files(tmp.path(), &[("b.rs", "two\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(repo.recent_diff(0).unwrap(), "");
    }

    #[test]
    fn recent_diff_zero_commits_is_empty_for_root_only_history() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "first\n")], &[]);
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
        commit_files(tmp.path(), &[("big.rs", big.as_str())], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        assert!(diff.ends_with("[diff truncated]\n"));
        assert!(diff.len() <= 512 * 1024 + "\n[diff truncated]\n".len());
    }

    #[test]
    fn recent_diff_converts_non_utf8_lines_lossily() {
        let tmp = TempDir::new().unwrap();
        let repo_git = git2::Repository::init(tmp.path()).unwrap();
        // No NUL byte, so git treats the file as text; line 2 is invalid UTF-8.
        std::fs::write(tmp.path().join("m.rs"), b"good1\nbad\xff\xfe\ngood2\n").unwrap();
        let mut index = repo_git.index().unwrap();
        index.add_path(Path::new("m.rs")).unwrap();
        index.write().unwrap();
        let tree = repo_git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        repo_git
            .commit(Some("HEAD"), &sig, &sig, "bin", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let diff = repo.recent_diff(1).unwrap();
        // The invalid line is kept with U+FFFD replacements; no marker.
        assert!(diff.contains("\n+good1\n"), "{diff}");
        assert!(diff.contains("\n+good2\n"), "{diff}");
        assert!(diff.contains("\n+bad\u{FFFD}\u{FFFD}\n"), "{diff}");
        assert!(!diff.contains("[diff truncated]"), "{diff}");
    }

    #[test]
    fn recent_diff_under_cap_has_no_truncation_marker() {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), &[("a.rs", "small\n")], &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert!(!repo.recent_diff(1).unwrap().contains("[diff truncated]"));
    }

    #[cfg(unix)]
    fn symlink_repo() -> TempDir {
        let tmp = TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("real.py"), "def real(): pass\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap(); // never added
        std::os::unix::fs::symlink(".env", tmp.path().join("a.py")).unwrap();
        std::os::unix::fs::symlink("real.py", tmp.path().join("b.py")).unwrap();
        let mut index = git.index().unwrap();
        for f in ["real.py", "a.py", "b.py"] {
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        tmp
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_symlink_to_untracked_file() {
        let tmp = symlink_repo();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("a.py")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
        assert!(err.to_string().contains("untracked"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_allows_symlink_to_tracked_file() {
        let tmp = symlink_repo();
        let repo = GitRepo::new(tmp.path()).unwrap();
        assert_eq!(
            repo.read_tracked(Path::new("b.py")).unwrap(),
            "def real(): pass\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_symlink_to_untracked_file() {
        let tmp = symlink_repo();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let names: Vec<String> = repo
            .scan()
            .unwrap()
            .into_iter()
            .map(|(p, ..)| p.to_string_lossy().into_owned())
            .collect();
        assert!(!names.contains(&"a.py".to_string()), "{names:?}");
        assert!(names.contains(&"b.py".to_string()), "{names:?}");
        assert!(names.contains(&"real.py".to_string()), "{names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_file_via_symlinked_directory_to_untracked() {
        let tmp = TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::create_dir(tmp.path().join("ignored_dir")).unwrap();
        std::fs::write(tmp.path().join("ignored_dir/secret.py"), "S=1\n").unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "ignored_dir/\n").unwrap();
        std::os::unix::fs::symlink("ignored_dir", tmp.path().join("docs")).unwrap();
        let mut index = git.index().unwrap();
        for f in [".gitignore", "docs"] {
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("docs/secret.py")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
    }

    #[cfg(unix)]
    fn commit_dir_link(link: &str, target: &str, extra_dir: Option<&str>) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=1\n").unwrap();
        std::fs::write(tmp.path().join(".gitignore"), ".env\n").unwrap();
        if let Some(d) = extra_dir {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        std::os::unix::fs::symlink(target, tmp.path().join(link)).unwrap();
        let mut index = git.index().unwrap();
        for f in [".gitignore", link] {
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        tmp
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_via_dir_symlink_to_root() {
        let tmp = commit_dir_link("docs", ".", None);
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("docs/.env")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_via_nested_dir_symlink_to_parent() {
        let tmp = commit_dir_link("a/b", "..", Some("a"));
        let repo = GitRepo::new(tmp.path()).unwrap();
        let err = repo.read_tracked(Path::new("a/b/.env")).unwrap_err();
        assert!(is_path_escape(&err), "{err}");
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
        let got: Vec<(&str, SymbolKind, u32)> = out
            .iter()
            .map(|(n, k, l)| (n.as_str(), k.clone(), *l))
            .collect();
        assert_eq!(
            got,
            vec![
                ("decorated", SymbolKind::Function, 4),
                ("inner", SymbolKind::Function, 5),
                ("InnerCls", SymbolKind::Class, 7),
                ("Decorated", SymbolKind::Class, 11),
                ("am", SymbolKind::Function, 12),
                ("top_async", SymbolKind::Function, 15),
            ]
        );
    }

    #[test]
    fn extract_python_import_forms() {
        let src = "import a.b as c\nfrom . import x\nfrom .. import y\nfrom a import (b,\n    c)\n\"\"\"\nimport in_doc\n\"\"\"\n# import in_comment\n";
        let out = SymbolExtractor::extract(Path::new("a.py"), src, Language::Python).unwrap();
        let got: Vec<(&str, u32)> = out
            .iter()
            .filter(|(_, k, _)| *k == SymbolKind::Import)
            .map(|(n, _, l)| (n.as_str(), *l))
            .collect();
        assert_eq!(
            got,
            vec![
                ("import a.b as c", 1),
                ("from . import x", 2),
                ("from .. import y", 3),
                ("from a import (b,\n    c)", 4),
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
        let lines: Vec<u32> = out
            .iter()
            .filter(|(_, k, _)| *k == SymbolKind::Struct)
            .map(|(_, _, l)| *l)
            .collect();
        assert_eq!(lines, vec![4, 5, 6, 9]);
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
    fn extract_small_stack(
        file: &str,
        src: String,
        lang: Language,
    ) -> Vec<(String, SymbolKind, u32)> {
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
