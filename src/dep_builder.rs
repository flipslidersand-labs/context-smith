use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Return true if the file name matches Go generated/test file patterns that
/// should be excluded from BFS traversal to avoid inflating the token budget.
///
/// Excluded patterns: `*_test.go`, `*.pb.go`, `mock_*.go`. (#91)
fn is_go_generated_or_test(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    name.ends_with("_test.go") || name.ends_with(".pb.go") || name.starts_with("mock_")
}

/// Build a directory-to-file-ids index from `file_paths` for O(1) Go import resolution.
/// Maps each file's parent directory path to the list of file IDs in that directory.
///
/// Only `.go` files are indexed (other files in the same directory are ignored).
/// Go test and generated files (`*_test.go`, `*.pb.go`, `mock_*.go`) are excluded so
/// they do not inflate the BFS frontier when `resolve_go` returns IDs for a directory. (#91)
fn build_go_dir_index(file_paths: &HashMap<i64, PathBuf>) -> HashMap<PathBuf, Vec<i64>> {
    let mut dir_to_files: HashMap<PathBuf, Vec<i64>> = HashMap::new();
    for (id, path) in file_paths {
        if path.extension().and_then(|e| e.to_str()) != Some("go") || is_go_generated_or_test(path)
        {
            continue;
        }
        if let Some(dir) = path.parent() {
            dir_to_files.entry(dir.to_path_buf()).or_default().push(*id);
        }
    }
    dir_to_files
}

/// Populate the deps table by resolving import symbols already stored in the DB.
/// Clears the table and rebuilds atomically inside a transaction.
///
/// `go_modules` are the `(directory, module path)` pairs of every `go.mod` in the repo (see
/// [`read_go_modules`]); when non-empty, Go imports resolve only inside those modules (#419, #429).
pub fn build_deps(
    conn: &Connection,
    file_paths: &HashMap<i64, PathBuf>,
    go_module: &[GoModule],
) -> Result<()> {
    let path_to_id: HashMap<PathBuf, i64> =
        file_paths.iter().map(|(id, p)| (p.clone(), *id)).collect();

    // Pre-build Go directory index once — O(N) — so resolve_go can do O(1) lookups.
    let go_dir_index = build_go_dir_index(file_paths);

    // Collect import rows before starting the write transaction
    let mut stmt = conn.prepare("SELECT file_id, name FROM symbols WHERE kind = 'import'")?;
    let imports: Vec<(i64, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        conn.execute_batch("DELETE FROM deps")?;
        for (from_id, import_str) in &imports {
            let from_path = match file_paths.get(from_id) {
                Some(p) => p,
                None => continue,
            };
            for to_id in
                resolve_import(import_str, from_path, &path_to_id, &go_dir_index, go_module)
            {
                if *from_id != to_id {
                    conn.execute(
                        "INSERT OR IGNORE INTO deps (from_file, to_file) VALUES (?1, ?2)",
                        params![from_id, to_id],
                    )?;
                }
            }
        }
        Ok(())
    })();

    match result {
        Ok(()) => crate::commit_or_rollback(conn).map_err(Into::into),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn resolve_import(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
    go_dir_index: &HashMap<PathBuf, Vec<i64>>,
    go_module: &[GoModule],
) -> Vec<i64> {
    let ext = from_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => resolve_rust(import_str, from_path, path_to_id),
        "py" => resolve_python(import_str, from_path, path_to_id),
        "go" => resolve_go(import_str, go_dir_index, go_module),
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => {
            resolve_ts_js(import_str, from_path, path_to_id)
        }
        "rb" => resolve_ruby(import_str, from_path, path_to_id),
        _ => vec![],
    }
}

/// Resolve a Ruby `require_relative 'x'` / `require 'x'` call to a file id.
/// The stored import text is the whole call (`require_relative '../x'`).
/// `require_relative` is resolved against the caller's directory; `require` only
/// matches `lib/x.rb` or `x.rb` inside the repo (gems are not resolved).
/// A path that climbs above the repo root (`normalize_path` → None) is ignored.
fn resolve_ruby(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Vec<i64> {
    let s = import_str.trim_start();
    let (relative, rest) = if let Some(r) = s.strip_prefix("require_relative") {
        (true, r)
    } else if let Some(r) = s.strip_prefix("require") {
        (false, r)
    } else {
        return vec![];
    };
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '(') {
        return vec![];
    }
    let Some(open) = rest.find(['\'', '"']) else {
        return vec![];
    };
    let quote = rest.as_bytes()[open] as char;
    let after = &rest[open + 1..];
    let Some(end) = after.find(quote) else {
        return vec![];
    };
    let spec = &after[..end];
    // `require './x'` / `require '../x'` are cwd-relative, not repo-resolvable.
    if spec.is_empty() || Path::new(spec).is_absolute() || (!relative && spec.starts_with('.')) {
        return vec![];
    }
    let file = if spec.ends_with(".rb") {
        spec.to_string()
    } else {
        format!("{spec}.rb")
    };
    let candidates: Vec<Option<PathBuf>> = if relative {
        let base = from_path.parent().unwrap_or(Path::new(""));
        vec![normalize_path(&base.join(&file))]
    } else {
        vec![
            normalize_path(&Path::new("lib").join(&file)),
            normalize_path(Path::new(&file)),
        ]
    };
    for c in candidates.into_iter().flatten() {
        if let Some(&id) = path_to_id.get(&c) {
            return vec![id];
        }
    }
    vec![]
}

/// Resolve an ES `import ... from './x'`, a re-export `export ... from './x'`, or CommonJS `require('./x')` to a file id.
/// Only relative specifiers (`./` or `../`) are resolved; bare package imports are ignored.
///
/// # Security (#87)
/// `normalize_path` performs only lexical `..` collapsing without filesystem access.
/// To prevent path-traversal attacks (e.g. `require('../../../etc/shadow')`),
/// `normalize_path` returns `None` when a `..` climbs above the repo root (paths in
/// the index are relative to it), and resolution then returns an empty result.
fn resolve_ts_js(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Vec<i64> {
    let spec = match extract_module_specifier(import_str) {
        Some(s) if s.starts_with('.') => s,
        _ => return vec![],
    };

    let base_dir = from_path.parent().unwrap_or(Path::new(""));
    let joined = normalize_path(&base_dir.join(&spec));

    // #87/#297: `normalize_path` returns None when `..` climbs above the repo root.
    let joined = match joined {
        Some(p) => p,
        None => return vec![],
    };

    // Try the specifier as-is (it may already carry an extension), then common
    // TS/JS source extensions and directory index files.
    const EXTS: [&str; 8] = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
    let mut candidates: Vec<PathBuf> = vec![joined.clone()];
    for ext in EXTS {
        candidates.push(PathBuf::from(format!("{}.{}", joined.display(), ext)));
    }
    // #189: TS ESM convention writes `./a.js` for a source file `a.ts`. Tried after the as-is and
    // appended-extension candidates (a real `a.js` wins) but before `index.*`
    // (so `dir.ts` beats `dir.js/index.ts`).
    let ts_alts: &[&str] = match joined.extension().and_then(|e| e.to_str()) {
        Some("js") => &["ts", "tsx"],
        Some("jsx") => &["tsx"],
        Some("mjs") => &["mts"],
        Some("cjs") => &["cts"],
        _ => &[],
    };
    for ext in ts_alts {
        candidates.push(joined.with_extension(ext));
    }

    for ext in EXTS {
        candidates.push(joined.join(format!("index.{}", ext)));
    }
    for candidate in candidates {
        if let Some(&id) = path_to_id.get(&candidate) {
            return vec![id];
        }
    }
    vec![]
}

/// Pull the quoted module path out of an import/export/require statement.
/// `import { a } from "./b"` → `./b`,  `require('./c')` → `./c`
///
/// Comments and string literals are tokenized first (#220), so a quote inside a
/// comment (`export /* 'cm' */ * from './x'`, `// don't`) or a string-literal
/// export name (`export { "n" as m } from './x'`) is not mistaken for the
/// specifier. The specifier is the string right after the last `from` keyword;
/// without one (`import './x'`, `require('./x')`) it is the first string.
fn extract_module_specifier(s: &str) -> Option<String> {
    enum Tok {
        Word(String),
        Str(String),
    }
    let chars: Vec<char> = s.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if matches!(c, '"' | '\'' | '`') {
            let mut j = i + 1;
            let mut lit = String::new();
            while j < chars.len() && chars[j] != c {
                if chars[j] == '\\' && j + 1 < chars.len() {
                    j += 1;
                }
                lit.push(chars[j]);
                j += 1;
            }
            if j >= chars.len() {
                return None; // unterminated string
            }
            toks.push(Tok::Str(lit));
            i = j + 1;
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            toks.push(Tok::Word(chars[start..i].iter().collect()));
        } else {
            i += 1;
        }
    }
    let after_from = toks.windows(2).rev().find_map(|w| match w {
        [Tok::Word(f), Tok::Str(spec)] if f == "from" => Some(spec.clone()),
        _ => None,
    });
    after_from.or_else(|| {
        toks.iter().find_map(|t| match t {
            Tok::Str(spec) => Some(spec.clone()),
            _ => None,
        })
    })
}

/// Collapse `.` and `..` segments in a path lexically (no filesystem access),
/// so `a/./b/../c` becomes `a/c`. Returns `None` if a `..` climbs above the
/// start of a relative path (i.e. escapes the repo root).
fn normalize_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Expand grouped Rust use paths: "crate::{a, b}" → ["crate::a", "crate::b"]
fn expand_rust_use(s: &str) -> Vec<String> {
    if let Some(brace) = s.find('{') {
        let prefix = &s[..brace];
        let inner_end = s.rfind('}').unwrap_or(s.len());
        let inner = &s[brace + 1..inner_end];
        split_brace_aware(inner)
            .flat_map(|part| expand_rust_use(&format!("{}{}", prefix, part.trim())))
            .collect()
    } else {
        let clean = s.split(" as ").next().unwrap_or(s).trim();
        let clean = clean.trim_end_matches("::*");
        vec![clean.to_string()]
    }
}

/// Split `s` on ',' but ignore commas inside nested `{...}`.
fn split_brace_aware(s: &str) -> impl Iterator<Item = &str> {
    let mut parts = Vec::new();
    let mut depth: usize = 0;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts.into_iter()
}

/// from_path の祖先のうち `<dir>/src/lib.rs` か `<dir>/src/main.rs` を持つ最も近いディレクトリ
/// をクレートルートとみなす。見つからなければリポジトリ直下("")。
fn rust_crate_root(from_path: &Path, path_to_id: &HashMap<PathBuf, i64>) -> PathBuf {
    from_path
        .ancestors()
        .skip(1)
        .find(|dir| {
            ["src/lib.rs", "src/main.rs"]
                .iter()
                .any(|f| path_to_id.contains_key(&dir.join(f)))
        })
        .map(Path::to_path_buf)
        .unwrap_or_default()
}

/// `mod foo;` を宣言ファイルの隣のモジュールファイルへ解決する。
/// lib.rs / main.rs / mod.rs は同じディレクトリ、それ以外の `a.rs` は `a/` を探す。
fn resolve_rust_mod_decl(
    name: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Vec<i64> {
    let dir = from_path.parent().unwrap_or(Path::new(""));
    let stem = from_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let has_src = |p: &Path| p.components().any(|c| c.as_os_str() == "src");
    let dir_name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let parent = dir.parent().unwrap_or(Path::new(""));
    let parent_name = parent.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let grandparent = parent.parent().unwrap_or(Path::new(""));
    let grandparent_name = grandparent
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    // Cargo の target ルート: lib.rs / main.rs / mod.rs、build.rs、tests/・examples/・benches/ 直下の *.rs、src/bin/*.rs
    // (src/bin/<name>/main.rs は main.rs で網羅)。ルート内の `mod x;` は同ディレクトリを探す。
    // build.rs と tests/・examples/・benches/ はパッケージ直下のみ(祖先に `src` があれば通常モジュール)、
    // src/bin は祖先にさらに `src` が無い場合のみ。
    // 対象外: `#[path]` 属性、bin から lib への `use <crate>::…` のエッジ、
    // `[lib] path` 等の非標準レイアウト(Cargo.toml を読まないため判別不可。リポジトリ直下の lib.rs / main.rs は通常モジュール)。
    // lib.rs / main.rs は `src/` 直下か `src/bin/<name>/`・`tests|examples|benches/<name>/` 直下のときだけルート
    // (それ以外の `a/lib.rs` は通常モジュール)。mod.rs は常に同ディレクトリ。
    let is_crate_file_root = match stem {
        "lib" => dir_name == "src",
        "main" => {
            dir_name == "src"
                || (matches!(parent_name, "tests" | "examples" | "benches") && !has_src(parent))
                || (parent_name == "bin"
                    && grandparent_name == "src"
                    && !has_src(grandparent.parent().unwrap_or(Path::new(""))))
        }
        _ => false,
    };
    let is_target_root = stem == "mod"
        || is_crate_file_root
        || (stem == "build" && !has_src(dir))
        || (matches!(dir_name, "tests" | "examples" | "benches") && !has_src(dir))
        || (dir_name == "bin"
            && parent_name == "src"
            && !has_src(parent.parent().unwrap_or(Path::new(""))));
    let name = name.strip_prefix("r#").unwrap_or(name);
    let base = if is_target_root {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    [
        base.join(format!("{name}.rs")),
        base.join(name).join("mod.rs"),
    ]
    .iter()
    .find_map(|c| path_to_id.get(c).copied())
    .into_iter()
    .collect()
}

fn resolve_rust(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Vec<i64> {
    if let Some(name) = import_str.strip_prefix("mod ") {
        return resolve_rust_mod_decl(name.trim(), from_path, path_to_id);
    }
    let root = rust_crate_root(from_path, path_to_id);
    let mut ids = Vec::new();
    for expanded in expand_rust_use(import_str) {
        let Some(tail) = expanded.strip_prefix("crate::") else {
            continue;
        };
        // 最長一致: 末尾が型・関数名の場合に備え、1セグメントずつ削って再試行する
        let mut segs: Vec<&str> = tail.split("::").collect();
        'search: while !segs.is_empty() {
            let rel = segs.join("/");
            for candidate in [
                root.join(format!("src/{}.rs", rel)),
                root.join(format!("src/{}/mod.rs", rel)),
            ] {
                if let Some(&id) = path_to_id.get(&candidate) {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                    break 'search;
                }
            }
            segs.pop();
        }
    }
    ids
}

fn resolve_python(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Vec<i64> {
    // 文全体(複数行・括弧・バックスラッシュ継続・コメント)を1行の空白区切りに正規化する
    let cleaned: String = import_str
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['(', ')', '\\'], " ");
    let cleaned = cleaned.trim();
    let base = from_path.parent().unwrap_or(Path::new(""));
    let mut ids: Vec<i64> = Vec::new();
    let mut push = |id: Option<i64>| {
        if let Some(id) = id {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    };
    // `X as Y` の X だけを取り出す
    let strip_as = |s: &str| s.split_whitespace().next().unwrap_or("").to_string();

    // キーワード直後は空白全般(タブ等)を許す
    let after_kw = |kw: &str| -> Option<String> {
        let rest = cleaned.strip_prefix(kw)?;
        rest.starts_with(char::is_whitespace)
            .then(|| rest.trim_start().to_string())
    };
    // base から `ups` 階層上がる。リポジトリルートを超えるなら None
    let ascend = |dot_count: usize| -> Option<&Path> {
        let mut dir = base;
        for _ in 1..dot_count {
            if dir.as_os_str().is_empty() {
                return None;
            }
            dir = dir.parent().unwrap_or(Path::new(""));
        }
        Some(dir)
    };

    if let Some(tail) = after_kw("from") {
        let dot_count = tail.chars().take_while(|&c| c == '.').count();
        let after_dots = &tail[dot_count..];
        // `from .import x` のように、ドット直後に空白なしで import が続く形も許す
        let (module, names) = match after_dots.strip_prefix("import") {
            Some(rest) if dot_count > 0 && rest.starts_with(char::is_whitespace) => ("", rest),
            _ => match after_dots.split_once(char::is_whitespace) {
                Some((m, rest)) => (m, rest.trim_start().strip_prefix("import").unwrap_or("")),
                None => (after_dots, ""),
            },
        };
        let Some(base_dir) = ascend(dot_count) else {
            return ids;
        };
        let rel = module.replace('.', "/");
        // 相対 import(ドットあり)ではリポジトリルート探索をしない
        let allow_root = dot_count == 0;
        if module.is_empty() {
            // `from .. import z` — パッケージ自身
            push(path_to_id.get(&base_dir.join("__init__.py")).copied());
        } else {
            push(lookup_py_module(&rel, base_dir, allow_root, path_to_id));
        }
        // `from X import a, b` — X/a.py, X/a/__init__.py などサブモジュール候補
        for name in names.split(',').map(strip_as) {
            if name.is_empty() || name == "*" || name.contains('.') {
                continue;
            }
            let sub = if module.is_empty() {
                name
            } else {
                format!("{}/{}", rel, name)
            };
            push(lookup_py_module(&sub, base_dir, allow_root, path_to_id));
        }
    } else if let Some(tail) = after_kw("import") {
        for part in tail.split(',').map(strip_as) {
            let dot_count = part.chars().take_while(|&c| c == '.').count();
            let module = part.trim_start_matches('.');
            if module.is_empty() {
                continue;
            }
            let Some(base_dir) = ascend(dot_count) else {
                continue;
            };
            push(lookup_py_module(
                &module.replace('.', "/"),
                base_dir,
                dot_count == 0,
                path_to_id,
            ));
        }
    }
    ids
}

/// `rel`(スラッシュ区切り)に対応する .py / __init__.py を探す。`allow_root` で
/// リポジトリルート相対のフォールバックも試す。
fn lookup_py_module(
    rel: &str,
    base_dir: &Path,
    allow_root: bool,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Option<i64> {
    let local = [
        base_dir.join(format!("{}.py", rel)),
        base_dir.join(format!("{}/__init__.py", rel)),
    ];
    let mut candidates = Vec::new();
    if allow_root {
        // 絶対 import は sys.path(ルート)側を先に探し、無ければ呼び出し元へフォールバック
        candidates.push(PathBuf::from(format!("{}.py", rel)));
        candidates.push(PathBuf::from(format!("{}/__init__.py", rel)));
    }
    candidates.extend(local);
    candidates
        .into_iter()
        .find_map(|c| path_to_id.get(&c).copied())
}

/// Resolve a Go import path to file IDs using a pre-built directory index.
///
/// The `go_dir_index` maps each file's parent directory (as stored in the repo)
/// to the file IDs within that directory. Resolution tries progressively shorter
/// suffixes of the import path until a unique match is found or all candidates
/// are exhausted. When multiple directories match the same suffix (name collision,
/// e.g. `internal/auth` vs `pkg/auth`), all candidates are returned so that the
/// caller can handle the ambiguity rather than silently picking the wrong one.
///
/// # Generated-file exclusion (#91)
/// `*_test.go`, `*.pb.go`, and `mock_*.go` files are excluded from the returned
/// IDs so they do not inflate the BFS frontier and consume token budget.
///
/// # Module scoping (#419)
/// With a non-empty `go_module` list (one entry per `go.mod`, #429), the module with the
/// longest path equal to or prefixing the import wins, and the remainder must match a repo
/// directory under that module's directory exactly (no suffix matching), so `net/http` or
/// external modules never attach to a same-named repo directory.
/// With an empty list (no readable `go.mod`) the legacy suffix match is kept.
fn resolve_go(
    import_str: &str,
    go_dir_index: &HashMap<PathBuf, Vec<i64>>,
    go_module: &[GoModule],
) -> Vec<i64> {
    // Single, grouped, aliased, dot and blank imports all reduce to quoted literals.
    let mut out: Vec<i64> = Vec::new();
    for pkg in go_import_paths(import_str) {
        for id in resolve_go_package(pkg, go_dir_index, go_module) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
    out
}

/// Extract every quoted import path from a Go `import` declaration, skipping `//` and `/* */` comments.
fn go_import_paths(src: &str) -> Vec<&str> {
    let mut paths = Vec::new();
    let mut in_block = false; // inside a `/* ... */` that spans lines
    for line in src.lines() {
        let mut rest = line;
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(end) => {
                        rest = &rest[end + 2..];
                        in_block = false;
                    }
                    None => break,
                }
            }
            let Some(pos) = rest.find(['"', '`', '/']) else {
                break;
            };
            let c = rest.as_bytes()[pos];
            if c == b'/' {
                if rest[pos..].starts_with("//") {
                    break;
                }
                if rest[pos..].starts_with("/*") {
                    in_block = true;
                    rest = &rest[pos + 2..];
                } else {
                    rest = &rest[pos + 1..];
                }
                continue;
            }
            let after = &rest[pos + 1..];
            match after.find(c as char) {
                Some(end) => {
                    paths.push(&after[..end]);
                    rest = &after[end + 1..];
                }
                None => break,
            }
        }
    }
    paths
}

/// A Go module: the repo-relative directory holding its `go.mod` and its `module` path.
pub type GoModule = (PathBuf, String);

/// Read the `module` path from `<dir>/go.mod`. Returns None if absent, a symlink,
/// unreadable, oversized, or without a `module` line.
fn read_go_mod_file(dir: &Path) -> Option<String> {
    let p = dir.join("go.mod");
    let meta = std::fs::symlink_metadata(&p).ok()?;
    if !meta.is_file() || meta.len() > 1 << 20 {
        return None;
    }
    parse_go_module(&std::fs::read_to_string(&p).ok()?)
}

/// Collect every `go.mod` that governs a tracked `.go` file: the repo root plus each
/// ancestor directory of a tracked Go file (#429). Only those directories are probed, and
/// only if the `go.mod` itself is in `tracked` (repo-relative paths); untracked or ignored
/// `go.mod` files are ignored like any other untracked file (#441).
pub fn read_go_modules(
    root: &Path,
    file_paths: &HashMap<i64, PathBuf>,
    tracked: &std::collections::HashSet<String>,
) -> Vec<GoModule> {
    let mut dirs: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for path in file_paths.values() {
        if path.extension().and_then(|e| e.to_str()) != Some("go") {
            continue;
        }
        let mut cur = path.parent();
        while let Some(d) = cur {
            if !dirs.insert(d.to_path_buf()) {
                break; // ancestors already recorded
            }
            cur = d.parent();
        }
    }
    dirs.insert(PathBuf::new());
    dirs.into_iter()
        .filter(|d| tracked.contains(&d.join("go.mod").to_string_lossy().to_string()))
        .filter_map(|d| read_go_mod_file(&root.join(&d)).map(|m| (d, m)))
        .collect()
}

fn parse_go_module(src: &str) -> Option<String> {
    for line in src.lines() {
        let line = line.split("//").next().unwrap_or("").trim();
        if let Some(rest) = line.strip_prefix("module") {
            if !rest.starts_with(char::is_whitespace) {
                continue;
            }
            let m = rest.trim();
            if m.starts_with('(') {
                return None; // block form `module ( ... )`: unsupported
            }
            let m = m.trim_matches(|c| c == '"' || c == '`');
            if !m.is_empty() {
                return Some(m.to_string());
            }
        }
    }
    None
}

// Known simplification (#441): a root-module import can also resolve into a child directory
// that has its own go.mod. The impact is small, so it is accepted rather than excluded.
fn resolve_go_package(
    pkg: &str,
    go_dir_index: &HashMap<PathBuf, Vec<i64>>,
    go_modules: &[GoModule],
) -> Vec<i64> {
    if pkg.is_empty() {
        return vec![];
    }

    if !go_modules.is_empty() {
        // Longest matching module path wins (nested modules shadow the parent).
        let mut best: Option<(&GoModule, &str)> = None;
        for gm in go_modules {
            let module = gm.1.trim_end_matches('/');
            let rest = if pkg == module {
                ""
            } else {
                match pkg.strip_prefix(module).and_then(|r| r.strip_prefix('/')) {
                    Some(r) => r,
                    None => continue,
                }
            };
            if best.is_none_or(|(b, _)| module.len() > b.1.trim_end_matches('/').len()) {
                best = Some((gm, rest));
            }
        }
        let Some((gm, rest)) = best else {
            return vec![];
        };
        let dir: PathBuf =
            gm.0.iter()
                .chain(
                    rest.split('/')
                        .filter(|s| !s.is_empty())
                        .map(std::ffi::OsStr::new),
                )
                .collect();
        return go_dir_index.get(&dir).cloned().unwrap_or_default();
    }

    let import_segs: Vec<&str> = pkg.split('/').filter(|s| !s.is_empty()).collect();
    if import_segs.is_empty() {
        return vec![];
    }

    // Try matching the full import path first, then progressively shorter suffixes.
    // This prefers more-specific (longer) matches over shorter ones, and avoids the
    // O(N×M) linear scan over all files.
    for depth in (1..=import_segs.len()).rev() {
        let suffix_segs = &import_segs[import_segs.len() - depth..];
        // Build the candidate directory path from suffix segments.
        let candidate_dir: PathBuf = suffix_segs.iter().collect();

        if let Some(ids) = go_dir_index.get(&candidate_dir) {
            // Found a matching directory entry in the index — O(1) lookup.
            // Return all file IDs in that directory, excluding generated/test files.
            return ids.clone();
        }
    }
    vec![]
}

/// BFS-expand from seed files through the deps graph (bidirectional, score decay × 0.5/hop).
///
/// # Batch query strategy (#91)
/// Instead of calling `query_neighbors` once per node (N×2 queries per depth level),
/// each BFS depth level collects the entire frontier and issues 2 SQL queries
/// per chunk of `NEIGHBOR_BATCH_CHUNK` ids (one `IN` for outgoing edges, one `IN` for
/// incoming edges), i.e. 2 × ceil(frontier / chunk) per level. This keeps round-trips
/// proportional to frontier/chunk rather than O(frontier), avoiding the N+1 problem of the original per-node approach.
pub fn bfs_expand(
    conn: &Connection,
    seeds: &[(i64, f32)],
    max_depth: u8,
) -> Result<HashMap<i64, f32>> {
    let mut scores: HashMap<i64, f32> = HashMap::new();

    for &(file_id, score) in seeds {
        let prev = scores.entry(file_id).or_insert(0.0);
        if score > *prev {
            *prev = score;
        }
    }

    // Build the frontier from the deduplicated max scores so duplicate seeds cannot
    // overwrite each other (last-wins) when collected into a map below. (#274)
    let mut frontier: Vec<(i64, f32)> = scores.iter().map(|(&id, &s)| (id, s)).collect();

    for _depth in 0..max_depth {
        if frontier.is_empty() {
            break;
        }

        // Collect all node IDs in this frontier for batch IN-clause queries.
        let ids: Vec<i64> = frontier.iter().map(|&(id, _)| id).collect();
        let frontier_scores: HashMap<i64, f32> = frontier.iter().copied().collect();

        // Batch-fetch every (parent, neighbour) edge of the frontier in 2 queries per chunk.
        let edges = query_neighbors_batch(conn, &ids)?;

        // Each neighbour gets the decayed score of its own best parent. (It used to get the
        // decayed score of the best node in the whole frontier, so every neighbour of any
        // seed outranked the weaker seeds. #135)
        let mut best: HashMap<i64, f32> = HashMap::new();
        for (parent, neighbor) in edges {
            let decayed = frontier_scores[&parent] * 0.5;
            let e = best.entry(neighbor).or_insert(f32::NEG_INFINITY);
            if decayed > *e {
                *e = decayed;
            }
        }

        let mut next_frontier: Vec<(i64, f32)> = Vec::new();
        for (neighbor_id, decayed) in best {
            let prev = scores.entry(neighbor_id).or_insert(0.0);
            if decayed > *prev {
                *prev = decayed;
                next_frontier.push((neighbor_id, decayed));
            }
        }

        frontier = next_frontier;
    }

    Ok(scores)
}

/// Max ids per IN clause in `query_neighbors_batch`.
const NEIGHBOR_BATCH_CHUNK: usize = 500;

/// Batch-fetch the edges that leave or enter a set of node IDs in 2 SQL queries per
/// chunk of `NEIGHBOR_BATCH_CHUNK` ids (one outgoing, one incoming), returned as `(parent, neighbour)` pairs where `parent` is the
/// node from `ids` and `neighbour` is the node on the other end, never itself in `ids`.
///
/// This replaces the per-node `query_neighbors` call, eliminating the N+1 pattern
/// that issued 2N queries for a frontier of N nodes. (#91) Keeping the parent lets the caller
/// decay each neighbour from its own parent's score. (#135)
fn query_neighbors_batch(conn: &Connection, ids: &[i64]) -> Result<Vec<(i64, i64)>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }

    let mut edges: HashSet<(i64, i64)> = HashSet::new();
    // Chunk the ids so each statement stays far below SQLite's bound-variable limit
    // (32766); the HashSet merges and de-duplicates results across chunks. (#273)
    for chunk in ids.chunks(NEIGHBOR_BATCH_CHUNK) {
        // Build the IN-clause placeholder string: "?1,?2,...,?N"
        let placeholders: String = (1..=chunk.len())
            .map(|i| format!("?{}", i))
            .collect::<Vec<_>>()
            .join(",");
        // Outgoing: parent → X, then incoming: X → parent.
        for sql in [
            format!(
                "SELECT from_file, to_file FROM deps WHERE from_file IN ({})",
                placeholders
            ),
            format!(
                "SELECT to_file, from_file FROM deps WHERE to_file IN ({})",
                placeholders
            ),
        ] {
            let mut stmt = conn.prepare_cached(&sql)?;
            let params_refs: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                edges.insert(row?);
            }
        }
    }

    // Exclude the input ids themselves so the BFS does not re-visit them.
    let id_set: HashSet<i64> = ids.iter().copied().collect();
    Ok(edges
        .into_iter()
        .filter(|(_, n)| !id_set.contains(n))
        .collect())
}

#[cfg(test)]
mod bfs_tests {
    use super::*;

    /// In-memory graph with the given `from -> to` import edges.
    fn graph(edges: &[(i64, i64)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE deps (from_file INTEGER NOT NULL, to_file INTEGER NOT NULL);",
        )
        .unwrap();
        for (a, b) in edges {
            conn.execute("INSERT INTO deps VALUES (?1, ?2)", [a, b])
                .unwrap();
        }
        conn
    }

    #[test]
    fn neighbour_gets_half_of_its_own_parent_score_not_the_best_seed() {
        // #135: file 3 is only linked to the weak seed 2. It used to get half of the *best*
        // seed's score (5.0) and outrank real seeds; it must get half of seed 2's (0.5).
        let conn = graph(&[(2, 3)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 1.0)], 1).unwrap();
        assert!((s[&3] - 0.5).abs() < 1e-6, "got {}", s[&3]);
        assert!((s[&1] - 10.0).abs() < 1e-6 && (s[&2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn incoming_edge_counts_too() {
        // 3 imports seed 2: the edge is 3 -> 2, so 3 is a neighbour via an incoming edge.
        let conn = graph(&[(3, 2)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 1.0)], 1).unwrap();
        assert!((s[&3] - 0.5).abs() < 1e-6, "got {}", s[&3]);
    }

    #[test]
    fn neighbour_of_two_seeds_takes_the_higher_decayed_score() {
        let conn = graph(&[(1, 3), (2, 3)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 4.0)], 1).unwrap();
        assert!((s[&3] - 5.0).abs() < 1e-6, "got {}", s[&3]);
    }

    #[test]
    fn score_halves_again_on_the_second_hop() {
        let conn = graph(&[(1, 2), (2, 3)]);
        let s = bfs_expand(&conn, &[(1, 8.0)], 2).unwrap();
        assert!(
            (s[&2] - 4.0).abs() < 1e-6 && (s[&3] - 2.0).abs() < 1e-6,
            "{s:?}"
        );
    }

    #[test]
    fn seeds_linked_to_each_other_keep_their_own_scores() {
        // Expansion only scores files outside the current frontier; it never rewrites a seed.
        let conn = graph(&[(1, 2)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 1.0)], 2).unwrap();
        assert!(
            (s[&1] - 10.0).abs() < 1e-6 && (s[&2] - 1.0).abs() < 1e-6,
            "{s:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(paths: &[(&str, i64)]) -> HashMap<PathBuf, i64> {
        paths
            .iter()
            .map(|(p, id)| (PathBuf::from(p), *id))
            .collect()
    }

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (file_id INTEGER, name TEXT, kind TEXT);
             CREATE TABLE deps (from_file INTEGER, to_file INTEGER,
                                PRIMARY KEY (from_file, to_file));",
        )
        .unwrap();
        conn
    }

    #[test]
    fn go_generated_and_test_files_detected() {
        assert!(is_go_generated_or_test(Path::new("a/x_test.go")));
        assert!(is_go_generated_or_test(Path::new("a/x.pb.go")));
        assert!(is_go_generated_or_test(Path::new("a/mock_x.go")));
        assert!(!is_go_generated_or_test(Path::new("a/x.go")));
    }

    #[test]
    fn go_dir_index_only_contains_go_files() {
        let paths: HashMap<i64, PathBuf> = [
            (1, PathBuf::from("pkg/auth/auth.go")),
            (2, PathBuf::from("pkg/auth/view.ts")),
            (3, PathBuf::from("pkg/auth/tool.py")),
            (4, PathBuf::from("pkg/auth/README")),
        ]
        .into();
        let idx = build_go_dir_index(&paths);
        assert_eq!(idx[&PathBuf::from("pkg/auth")], vec![1]);
        assert_eq!(resolve_go("\"x/pkg/auth\"", &idx, &[]), vec![1]);
    }

    #[test]
    fn go_dir_index_skips_generated_files() {
        let paths: HashMap<i64, PathBuf> = [
            (1, PathBuf::from("pkg/auth/auth.go")),
            (2, PathBuf::from("pkg/auth/auth_test.go")),
        ]
        .into();
        let idx = build_go_dir_index(&paths);
        assert_eq!(idx[&PathBuf::from("pkg/auth")], vec![1]);
    }

    #[test]
    fn resolve_go_prefers_longest_suffix_and_handles_empty() {
        let idx: HashMap<PathBuf, Vec<i64>> = [
            (PathBuf::from("internal/auth"), vec![1]),
            (PathBuf::from("auth"), vec![2]),
        ]
        .into();
        assert_eq!(
            resolve_go("\"example.com/app/internal/auth\"", &idx, &[]),
            vec![1]
        );
        assert_eq!(resolve_go("import \"other/auth\"", &idx, &[]), vec![2]);
        assert!(resolve_go("\"example.com/none\"", &idx, &[]).is_empty());
        assert!(resolve_go("\"\"", &idx, &[]).is_empty());
        assert!(resolve_go("\"/\"", &idx, &[]).is_empty());
    }

    #[test]
    fn resolve_go_handles_group_alias_and_dot_imports() {
        let idx: HashMap<PathBuf, Vec<i64>> = [
            (PathBuf::from("internal/auth"), vec![1]),
            (PathBuf::from("pkg/util"), vec![2, 3]),
            (PathBuf::from("db"), vec![4]),
        ]
        .into();
        let group = "import (\n\t\"fmt\"\n\t\"example.com/app/internal/auth\"\n\tu \"example.com/app/pkg/util\"\n)";
        assert_eq!(resolve_go(group, &idx, &[]), vec![1, 2, 3]);
        assert_eq!(
            resolve_go("import foo \"x/internal/auth\"", &idx, &[]),
            vec![1]
        );
        assert_eq!(resolve_go("import . \"x/db\"", &idx, &[]), vec![4]);
        assert_eq!(resolve_go("import _ \"x/db\"", &idx, &[]), vec![4]);
        // duplicates and comments are ignored
        let dup = "import (\n\t\"a/db\" // \"x/internal/auth\"\n\td \"b/db\"\n)";
        assert_eq!(resolve_go(dup, &idx, &[]), vec![4]);
        assert!(resolve_go("import (\n)", &idx, &[]).is_empty());
    }

    #[test]
    fn resolve_go_module_scopes_to_module_prefix() {
        let idx: HashMap<PathBuf, Vec<i64>> = [
            (PathBuf::from("http"), vec![1]),
            (PathBuf::from("internal/auth"), vec![2]),
            (PathBuf::from(""), vec![3]),
        ]
        .into();
        let m = &[(PathBuf::new(), "example.com/app".to_string())][..];
        // stdlib / external imports never attach to same-named repo dirs
        assert!(resolve_go("\"net/http\"", &idx, m).is_empty());
        assert!(resolve_go("\"github.com/other/internal/auth\"", &idx, m).is_empty());
        assert!(resolve_go("\"example.com/application/http\"", &idx, m).is_empty());
        // module-internal imports resolve by exact directory
        assert_eq!(
            resolve_go("\"example.com/app/internal/auth\"", &idx, m),
            vec![2]
        );
        assert_eq!(resolve_go("\"example.com/app/http\"", &idx, m), vec![1]);
        assert_eq!(resolve_go("\"example.com/app\"", &idx, m), vec![3]);
        assert!(resolve_go("\"example.com/app/auth\"", &idx, m).is_empty());
        // without go.mod the legacy suffix match is kept
        assert_eq!(resolve_go("\"net/http\"", &idx, &[]), vec![1]);
    }

    #[test]
    fn parse_go_module_variants() {
        assert_eq!(
            parse_go_module("module a.b/c\n\ngo 1.21\n").as_deref(),
            Some("a.b/c")
        );
        assert_eq!(
            parse_go_module("// x\nmodule \"a.b/c\" // hi\n").as_deref(),
            Some("a.b/c")
        );
        assert_eq!(parse_go_module("modulex a\ngo 1.21\n"), None);
        assert_eq!(parse_go_module(""), None);
        // block form is unsupported: fall back to legacy suffix matching
        assert_eq!(parse_go_module("module (\n\texample.com/app\n)\n"), None);
        assert_eq!(parse_go_module("module(\n\texample.com/app\n)\n"), None);
    }

    #[test]
    fn resolve_go_nested_modules_longest_match() {
        let idx: HashMap<PathBuf, Vec<i64>> = [
            (PathBuf::from("http"), vec![1]),
            (PathBuf::from("svc/util"), vec![2]),
            (PathBuf::from("svc"), vec![3]),
            (PathBuf::from("util"), vec![4]),
        ]
        .into();
        let mods = vec![
            (PathBuf::from("svc"), "github.com/o/other".to_string()),
            (PathBuf::new(), "example.com/app".to_string()),
        ];
        // nested module unrelated to the root path resolves under its own directory
        assert_eq!(
            resolve_go("\"github.com/o/other/util\"", &idx, &mods),
            vec![2]
        );
        assert_eq!(resolve_go("\"github.com/o/other\"", &idx, &mods), vec![3]);
        // root module still resolves from the repo root
        assert_eq!(resolve_go("\"example.com/app/util\"", &idx, &mods), vec![4]);
        // stdlib never attaches
        assert!(resolve_go("\"net/http\"", &idx, &mods).is_empty());
        // go.work-style layout (no root go.mod): only nested modules, still scoped
        let ws = &mods[..1];
        assert!(resolve_go("\"net/http\"", &idx, ws).is_empty());
        assert_eq!(resolve_go("\"github.com/o/other/util\"", &idx, ws), vec![2]);
        // a nested module whose path extends the root module's wins (longest match)
        let nested = vec![
            (PathBuf::new(), "example.com/app".to_string()),
            (PathBuf::from("svc"), "example.com/app/sub".to_string()),
        ];
        assert_eq!(
            resolve_go("\"example.com/app/sub/util\"", &idx, &nested),
            vec![2]
        );
    }

    #[test]
    fn read_go_modules_finds_root_and_nested_go_mod() {
        let d = std::env::temp_dir().join(format!("cs429-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("svc/inner")).unwrap();
        std::fs::create_dir_all(d.join("other")).unwrap();
        let paths: HashMap<i64, PathBuf> = [
            (1, PathBuf::from("svc/inner/a.go")),
            (2, PathBuf::from("other/b.go")),
            (3, PathBuf::from("c.rs")),
        ]
        .into();
        let tracked: std::collections::HashSet<String> =
            ["go.mod", "svc/go.mod", "svc/inner/go.mod"]
                .map(String::from)
                .into();
        assert!(read_go_modules(&d, &paths, &tracked).is_empty());
        std::fs::write(d.join("go.mod"), "module example.com/app\n").unwrap();
        std::fs::write(d.join("svc/go.mod"), "module github.com/o/other\n").unwrap();
        // block form ignored; unrelated dir without go.mod skipped
        std::fs::write(d.join("svc/inner/go.mod"), "module (\n\tx\n)\n").unwrap();
        // an untracked go.mod is ignored (#441)
        std::fs::write(d.join("other/go.mod"), "module example.com/untracked\n").unwrap();
        let mut got = read_go_modules(&d, &paths, &tracked);
        got.sort();
        assert_eq!(
            got,
            vec![
                (PathBuf::new(), "example.com/app".to_string()),
                (PathBuf::from("svc"), "github.com/o/other".to_string()),
            ]
        );
        // root go.mod not tracked: ignored too
        let only_svc: std::collections::HashSet<String> = ["svc/go.mod".to_string()].into();
        assert_eq!(
            read_go_modules(&d, &paths, &only_svc),
            vec![(PathBuf::from("svc"), "github.com/o/other".to_string())]
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn resolve_go_skips_block_comments() {
        let idx: HashMap<PathBuf, Vec<i64>> = [
            (PathBuf::from("internal/auth"), vec![1]),
            (PathBuf::from("db"), vec![4]),
        ]
        .into();
        // single-line block comment
        let single = "import (\n\t/* \"x/internal/auth\" */ \"a/db\"\n)";
        assert_eq!(resolve_go(single, &idx, &[]), vec![4]);
        // multi-line block comment
        let multi = "import (\n\t/* old:\n\t\"x/internal/auth\"\n\t*/\n\t\"a/db\"\n)";
        assert_eq!(resolve_go(multi, &idx, &[]), vec![4]);
        // import after the closing marker on the same line
        let after = "import (\n\t/* c\n\t*/ \"x/internal/auth\"\n)";
        assert_eq!(resolve_go(after, &idx, &[]), vec![1]);
        // `/*` inside string literals is not a comment
        let in_str = "import (\n\t\"a/*b\"\n\t`c/*d`\n\t\"x/internal/auth\"\n)";
        assert_eq!(resolve_go(in_str, &idx, &[]), vec![1]);
        // `//` inside a block comment does not end it; unterminated does not panic
        let nested = "import (\n\t/* // \"x/internal/auth\" */ \"a/db\"\n)";
        assert_eq!(resolve_go(nested, &idx, &[]), vec![4]);
        assert!(resolve_go("import (\n\t/* \"x/internal/auth\"\n", &idx, &[]).is_empty());
    }

    #[test]
    fn module_specifier_extraction() {
        assert_eq!(
            extract_module_specifier("import { a } from \"./b\""),
            Some("./b".into())
        );
        assert_eq!(
            extract_module_specifier("require('./c')"),
            Some("./c".into())
        );
        assert_eq!(extract_module_specifier("no quotes"), None);
        assert_eq!(extract_module_specifier("\"unterminated"), None);
    }

    #[test]
    fn module_specifier_skips_comments_and_string_names() {
        let want =
            |s: &str, e: &str| assert_eq!(extract_module_specifier(s), Some(e.into()), "{s}");
        // #220 forms
        want("export /* 'cm' */ * from './cmt'", "./cmt");
        want("export {\n  a, // don't\n  b,\n} from './multi'", "./multi");
        want(
            "export { \"str-name\" as sn } from './strname'",
            "./strname",
        );
        want("export { \"from\" as f } from './x'", "./x");
        want("export { from as f } from './x'", "./x");
        // import counterparts
        want("import /* 'cm' */ a from './cmt'", "./cmt");
        want("import {\n  a, // don't\n} from \"./imp\"", "./imp");
        want(
            "import { \"str-name\" as sn } from './strname'",
            "./strname",
        );
        // unchanged forms
        want("import './x'", "./x");
        want("import /* from 'no' */ './side'", "./side");
        want("import a from \"x\"", "x");
        want("export * from 'y'", "y");
        want("export * as ns from 'y'", "y");
    }

    #[test]
    fn resolve_ts_js_uses_specifier_after_from() {
        let ids: HashMap<PathBuf, i64> = [(PathBuf::from("src/cmt.ts"), 7)].into();
        let from = Path::new("src/a.ts");
        for stmt in [
            "export /* 'cm' */ * from './cmt'",
            "export {\n // don't\n a } from './cmt'",
            "import { \"s\" as t } from './cmt'",
        ] {
            assert_eq!(resolve_ts_js(stmt, from, &ids), vec![7], "{stmt}");
        }
    }

    #[test]
    fn normalize_path_collapses_dots() {
        assert_eq!(
            normalize_path(Path::new("a/./b/../c")),
            Some(PathBuf::from("a/c"))
        );
        assert_eq!(normalize_path(Path::new("../x")), None);
        assert_eq!(normalize_path(Path::new("a/../../b")), None);
    }

    #[test]
    fn expand_rust_use_handles_groups_nesting_and_alias() {
        assert_eq!(expand_rust_use("crate::a::b"), vec!["crate::a::b"]);
        assert_eq!(
            expand_rust_use("crate::{a, b::{c, d}}"),
            vec!["crate::a", "crate::b::c", "crate::b::d"]
        );
        assert_eq!(expand_rust_use("crate::a as z"), vec!["crate::a"]);
        assert_eq!(expand_rust_use("crate::a::*"), vec!["crate::a"]);
    }

    #[test]
    fn resolve_rust_finds_file_and_mod_rs() {
        let map = ids(&[("src/a.rs", 1), ("src/b/mod.rs", 2)]);
        assert_eq!(
            resolve_rust("crate::{a, b}", Path::new("src/lib.rs"), &map),
            vec![1, 2]
        );
        assert!(resolve_rust("std::fmt", Path::new("src/lib.rs"), &map).is_empty());
    }

    #[test]
    fn resolve_rust_drops_trailing_item_segments() {
        let map = ids(&[("src/budget.rs", 1), ("src/a/mod.rs", 2), ("src/a/b.rs", 3)]);
        assert_eq!(
            resolve_rust("crate::budget::Candidate", Path::new("src/lib.rs"), &map),
            vec![1]
        );
        assert_eq!(
            resolve_rust("crate::a::Item::method", Path::new("src/lib.rs"), &map),
            vec![2]
        );
        // 最長一致: a::b があれば a より優先する
        assert_eq!(
            resolve_rust("crate::a::b::Thing", Path::new("src/lib.rs"), &map),
            vec![3]
        );
        // 同じ親に解決される複数 item は重複しない
        assert_eq!(
            resolve_rust("crate::a::{x, y}", Path::new("src/lib.rs"), &map),
            vec![2]
        );
        assert!(resolve_rust("std::fmt", Path::new("src/lib.rs"), &map).is_empty());
        assert!(resolve_rust("crate::missing::Item", Path::new("src/lib.rs"), &map).is_empty());
    }

    #[test]
    fn resolve_rust_mod_decl_finds_sibling_module_files() {
        let map = ids(&[
            ("src/lib.rs", 1),
            ("src/a.rs", 2),
            ("src/b/mod.rs", 3),
            ("src/a/c.rs", 4),
            ("src/b/d.rs", 5),
        ]);
        let go = HashMap::new();
        let r = |from: &str, imp: &str| resolve_import(imp, Path::new(from), &map, &go, &[]);
        assert_eq!(r("src/lib.rs", "mod a"), vec![2]);
        assert_eq!(r("src/lib.rs", "mod b"), vec![3]);
        // 非 mod.rs のファイルは `a/` の下を探す
        assert_eq!(r("src/a.rs", "mod c"), vec![4]);
        assert_eq!(r("src/b/mod.rs", "mod d"), vec![5]);
        assert!(r("src/lib.rs", "mod missing").is_empty());
    }

    #[test]
    fn resolve_rust_mod_decl_handles_cargo_target_roots() {
        let map = ids(&[
            ("tests/it.rs", 1),
            ("tests/common.rs", 2),
            ("tests/util/mod.rs", 3),
            ("src/bin/tool.rs", 4),
            ("src/bin/helper.rs", 5),
            ("src/bin/multi/main.rs", 6),
            ("src/bin/multi/part.rs", 7),
            ("build.rs", 8),
            ("gen.rs", 9),
            ("tests/it/sub.rs", 10),
        ]);
        let go = HashMap::new();
        let r = |from: &str, imp: &str| resolve_import(imp, Path::new(from), &map, &go, &[]);
        assert_eq!(r("tests/it.rs", "mod common"), vec![2]);
        assert_eq!(r("tests/it.rs", "mod util"), vec![3]);
        assert_eq!(r("src/bin/tool.rs", "mod helper"), vec![5]);
        assert_eq!(r("src/bin/multi/main.rs", "mod part"), vec![7]);
        assert_eq!(r("build.rs", "mod gen"), vec![9]);
        // tests/it.rs は crate ルートなので tests/it/ ではなく tests/ を探す
        assert!(r("tests/it.rs", "mod sub").is_empty());
    }

    #[test]
    fn resolve_rust_mod_decl_handles_examples_and_benches_roots() {
        let map = ids(&[
            ("examples/e.rs", 1),
            ("examples/helper.rs", 2),
            ("examples/e/sub.rs", 3),
            ("benches/b.rs", 4),
            ("benches/util/mod.rs", 5),
            ("crates/x/examples/e.rs", 6),
            ("crates/x/examples/h.rs", 7),
            ("src/examples/foo.rs", 8),
            ("src/examples/foo/x.rs", 9),
            ("src/examples/x.rs", 10),
            ("src/benches/bar.rs", 11),
            ("src/benches/bar/y.rs", 12),
            ("src/benches/y.rs", 13),
        ]);
        let go = HashMap::new();
        let r = |from: &str, imp: &str| resolve_import(imp, Path::new(from), &map, &go, &[]);
        assert_eq!(r("examples/e.rs", "mod helper"), vec![2]);
        assert!(r("examples/e.rs", "mod sub").is_empty());
        assert_eq!(r("benches/b.rs", "mod util"), vec![5]);
        assert_eq!(r("crates/x/examples/e.rs", "mod h"), vec![7]);
        // src 配下の examples/benches は通常モジュール
        assert_eq!(r("src/examples/foo.rs", "mod x"), vec![9]);
        assert_eq!(r("src/benches/bar.rs", "mod y"), vec![12]);
    }

    #[test]
    fn resolve_rust_mod_decl_target_roots_ignore_nested_src_modules() {
        let map = ids(&[
            ("src/tests/foo.rs", 1),
            ("src/tests/foo/x.rs", 2),
            ("src/tests/x.rs", 3),
            ("src/foo/tests/y.rs", 4),
            ("src/foo/tests/y/z.rs", 5),
            ("src/foo/tests/z.rs", 6),
            ("src/build.rs", 7),
            ("src/build/x.rs", 8),
            ("src/x.rs", 9),
            ("src/foo/build.rs", 10),
            ("src/foo/build/w.rs", 11),
            ("src/foo/w.rs", 12),
            ("crates/x/tests/t.rs", 13),
            ("crates/x/tests/u.rs", 14),
            ("crates/x/build.rs", 15),
            ("crates/x/g.rs", 16),
            ("crates/x/src/bin/b.rs", 17),
            ("crates/x/src/bin/c.rs", 18),
            ("src/a/src/bin/d.rs", 19),
            ("src/a/src/bin/d/e.rs", 20),
            ("src/a/src/bin/e.rs", 21),
        ]);
        let go = HashMap::new();
        let r = |from: &str, imp: &str| resolve_import(imp, Path::new(from), &map, &go, &[]);
        assert_eq!(r("src/tests/foo.rs", "mod x"), vec![2]);
        assert_eq!(r("src/foo/tests/y.rs", "mod z"), vec![5]);
        assert_eq!(r("src/build.rs", "mod x"), vec![8]);
        assert_eq!(r("src/foo/build.rs", "mod w"), vec![11]);
        assert_eq!(r("crates/x/tests/t.rs", "mod u"), vec![14]);
        assert_eq!(r("crates/x/build.rs", "mod g"), vec![16]);
        assert_eq!(r("crates/x/src/bin/b.rs", "mod c"), vec![18]);
        assert_eq!(r("src/a/src/bin/d.rs", "mod e"), vec![20]);
    }

    #[test]
    fn resolve_rust_mod_decl_lib_main_outside_crate_root_are_normal_modules() {
        let map = ids(&[
            ("src/a/lib.rs", 1),
            ("src/a/lib/y.rs", 2),
            ("src/a/y.rs", 3),
            ("src/x/main.rs", 4),
            ("src/x/main/z.rs", 5),
            ("src/x/z.rs", 6),
            ("crates/c/src/lib.rs", 7),
            ("crates/c/src/q.rs", 8),
            ("src/bin/multi/main.rs", 9),
            ("src/bin/multi/part.rs", 10),
            ("src/tests/foo/main.rs", 11),
            ("src/tests/foo/main/w.rs", 12),
            ("src/tests/foo/w.rs", 13),
            ("tests/foo/main.rs", 14),
            ("tests/foo/v.rs", 15),
        ]);
        let go = HashMap::new();
        let r = |from: &str, imp: &str| resolve_import(imp, Path::new(from), &map, &go, &[]);
        assert_eq!(r("src/a/lib.rs", "mod y"), vec![2]);
        assert_eq!(r("src/x/main.rs", "mod z"), vec![5]);
        assert_eq!(r("crates/c/src/lib.rs", "mod q"), vec![8]);
        assert_eq!(r("src/bin/multi/main.rs", "mod part"), vec![10]);
        assert_eq!(r("src/tests/foo/main.rs", "mod w"), vec![12]);
        assert_eq!(r("tests/foo/main.rs", "mod v"), vec![15]);
    }

    #[test]
    fn resolve_rust_mod_decl_prefers_file_over_mod_rs() {
        let map = ids(&[("src/lib.rs", 1), ("src/foo.rs", 2), ("src/foo/mod.rs", 3)]);
        let go = HashMap::new();
        assert_eq!(
            resolve_import("mod foo", Path::new("src/lib.rs"), &map, &go, &[]),
            vec![2]
        );
    }

    #[test]
    fn resolve_rust_mod_decl_strips_raw_identifier_prefix() {
        let map = ids(&[("src/lib.rs", 1), ("src/type.rs", 2), ("src/fn/mod.rs", 3)]);
        let go = HashMap::new();
        let r = |imp: &str| resolve_import(imp, Path::new("src/lib.rs"), &map, &go, &[]);
        assert_eq!(r("mod r#type"), vec![2]);
        assert_eq!(r("mod r#fn"), vec![3]);
    }

    #[test]
    fn resolve_rust_uses_nearest_crate_root_in_workspace() {
        let map = ids(&[
            ("src/lib.rs", 10),
            ("src/x.rs", 11),
            ("crates/foo/src/lib.rs", 20),
            ("crates/foo/src/x.rs", 21),
            ("crates/foo/src/m/mod.rs", 22),
            ("crates/bar/src/main.rs", 30),
            ("crates/bar/src/y.rs", 31),
        ]);
        let go = HashMap::new();
        let r = |from: &str, imp: &str| resolve_import(imp, Path::new(from), &map, &go, &[]);
        assert_eq!(r("crates/foo/src/lib.rs", "crate::x"), vec![21]);
        assert_eq!(r("crates/foo/src/m/mod.rs", "crate::x::T"), vec![21]);
        assert_eq!(r("crates/bar/src/main.rs", "crate::y"), vec![31]);
        // 別クレートの src/x.rs には接続しない
        assert!(r("crates/bar/src/main.rs", "crate::x").is_empty());
        // ルート直下の src/ は従来通り
        assert_eq!(r("src/lib.rs", "crate::x"), vec![11]);
    }

    #[test]
    fn resolve_python_relative_and_absolute() {
        let map = ids(&[
            ("pkg/sub/mod.py", 1),
            ("pkg/util.py", 2),
            ("pkg/__init__.py", 3),
            ("top/__init__.py", 4),
        ]);
        let from = Path::new("pkg/sub/main.py");
        assert_eq!(resolve_python("from .mod import x", from, &map), vec![1]);
        assert_eq!(resolve_python("from ..util import y", from, &map), vec![2]);
        assert_eq!(resolve_python("from .. import z", from, &map), vec![3]);
        assert_eq!(resolve_python("import top", from, &map), vec![4]);
        // 同ディレクトリに z.py が無ければ空(下の resolve_python_submodules で解決を確認)
        assert!(resolve_python("from . import z", from, &map).is_empty());
        assert!(resolve_python("garbage", from, &map).is_empty());
        assert!(resolve_python("import nope", from, &map).is_empty());
    }

    #[test]
    fn resolve_python_submodules_and_comma_lists() {
        let map = ids(&[
            ("pkg/__init__.py", 1),
            ("pkg/helper.py", 2),
            ("pkg/other/__init__.py", 3),
            ("pkg/main.py", 4),
            ("pkg/z.py", 5),
            ("a.py", 6),
            ("b.py", 7),
            ("c/d.py", 8),
        ]);
        let from = Path::new("pkg/main.py");
        // サブモジュール import: X 自体(__init__) と X/name.py の両方
        assert_eq!(
            resolve_python("from pkg import helper", from, &map),
            vec![1, 2]
        );
        assert_eq!(
            resolve_python("from pkg import helper, other", from, &map),
            vec![1, 2, 3]
        );
        // 括弧・複数行・as・コメント
        assert_eq!(
            resolve_python(
                "from pkg import (\n    helper as h,  # c\n    other,\n)",
                from,
                &map
            ),
            vec![1, 2, 3]
        );
        // `*` は名前として扱わない
        assert_eq!(resolve_python("from pkg import *", from, &map), vec![1]);
        // 相対: パッケージ自身(__init__)と同ディレクトリの z.py
        assert_eq!(resolve_python("from . import z", from, &map), vec![1, 5]);
        // カンマ区切り import と as
        assert_eq!(resolve_python("import a, b", from, &map), vec![6, 7]);
        assert_eq!(
            resolve_python("import a as x, b as y", from, &map),
            vec![6, 7]
        );
        assert_eq!(resolve_python("import c.d as e", from, &map), vec![8]);
        // 重複は除く
        assert_eq!(resolve_python("import a, a", from, &map), vec![6]);
    }

    #[test]
    fn resolve_python_relative_submodule_does_not_fall_back_to_root() {
        let map = ids(&[
            ("pkg/main.py", 1),
            ("pkg/mod.py", 2),
            ("mod/x.py", 3),
            ("pkg/__init__.py", 4),
            ("pkg/helper.py", 5),
            ("lib/y.py", 6),
        ]);
        let from = Path::new("pkg/main.py");
        // 相対 import ではルート直下の mod/x.py に解決されない
        assert_eq!(resolve_python("from .mod import x", from, &map), vec![2]);
        // 絶対 import は従来どおりルートから解決される
        assert_eq!(
            resolve_python("from pkg import helper", from, &map),
            vec![4, 5]
        );
        assert_eq!(resolve_python("from lib import y", from, &map), vec![6]);
    }

    fn py_boundary_map() -> HashMap<PathBuf, i64> {
        ids(&[
            ("pkg/main.py", 1),
            ("pkg/mod.py", 2),
            ("mod/x.py", 3),
            ("pkg/__init__.py", 4),
            ("pkg/helper.py", 5),
            ("import.py", 6),
            ("z.py", 7),
        ])
    }

    #[test]
    fn resolve_python_absolute_import_prefers_root_over_caller_dir() {
        let from = Path::new("pkg/main.py");
        let map = ids(&[("util.py", 1), ("pkg/util.py", 2), ("only_pkg.py", 9)]);
        // 絶対 import はルートの util.py を優先する
        assert_eq!(resolve_python("import util", from, &map), vec![1]);
        assert_eq!(resolve_python("from util import x", from, &map), vec![1]);
        // 相対 import は従来どおり呼び出し元ディレクトリ
        assert_eq!(resolve_python("from .util import x", from, &map), vec![2]);
        // ルートに無ければ呼び出し元ディレクトリへフォールバック
        let map = ids(&[("pkg/helper.py", 3)]);
        assert_eq!(resolve_python("import helper", from, &map), vec![3]);
    }

    #[test]
    fn resolve_python_relative_module_has_no_root_search() {
        let from = Path::new("pkg/main.py");
        // pkg/mod.py が無く、ルートに mod/__init__.py だけある場合は解決しない
        let map = ids(&[("pkg/main.py", 1), ("mod/__init__.py", 3)]);
        assert!(resolve_python("from .mod import x", from, &map).is_empty());
        // pkg/mod.py があればそれだけ
        assert_eq!(
            resolve_python("from .mod import x", from, &py_boundary_map()),
            vec![2]
        );
    }

    #[test]
    fn resolve_python_from_dot_import_without_space() {
        let from = Path::new("pkg/main.py");
        assert_eq!(
            resolve_python("from .import helper", from, &py_boundary_map()),
            vec![4, 5]
        );
    }

    #[test]
    fn resolve_python_dots_above_root_do_not_resolve() {
        let from = Path::new("pkg/main.py");
        let map = py_boundary_map();
        assert!(resolve_python("from ... import z", from, &map).is_empty());
        assert!(resolve_python("from ...z import q", from, &map).is_empty());
        // `..` はルート直下までなので有効
        assert_eq!(resolve_python("from .. import z", from, &map), vec![7]);
    }

    #[test]
    fn resolve_python_accepts_any_whitespace_after_keywords() {
        let from = Path::new("pkg/main.py");
        assert_eq!(
            resolve_python("from\tpkg import helper", from, &py_boundary_map()),
            vec![4, 5]
        );
        assert_eq!(
            resolve_python("import\ttop", from, &ids(&[("top.py", 9)])),
            vec![9]
        );
    }

    #[test]
    fn resolve_ts_js_extensions_index_and_traversal() {
        let map = ids(&[("src/a.ts", 1), ("src/dir/index.js", 2), ("src/x.ts", 3)]);
        let from = Path::new("src/x.ts");
        assert_eq!(resolve_ts_js("import a from './a'", from, &map), vec![1]);
        assert_eq!(resolve_ts_js("import a from './a.ts'", from, &map), vec![1]);
        assert_eq!(resolve_ts_js("require('./dir')", from, &map), vec![2]);
        assert!(resolve_ts_js("import r from 'react'", from, &map).is_empty());
        assert!(resolve_ts_js("require('../../etc/shadow')", from, &map).is_empty());
        assert!(resolve_ts_js("require('./missing')", from, &map).is_empty());
    }

    #[test]
    fn resolve_ts_js_root_escape_does_not_hit_repo_file() {
        // #297: escaping above the repo root must not resolve to an in-repo `etc/x.ts`.
        let map = ids(&[("etc/x.ts", 1), ("b.ts", 2), ("src/x.ts", 3), ("a.ts", 4)]);
        let from = Path::new("src/x.ts");
        assert!(resolve_ts_js("import x from '../../../etc/x'", from, &map).is_empty());
        assert!(resolve_ts_js("import b from '../../b'", from, &map).is_empty());
        // From a root-level file, any `..` escapes.
        assert!(resolve_ts_js("import x from '../etc/x'", Path::new("a.ts"), &map).is_empty());
        // Staying exactly at the root still resolves.
        assert_eq!(resolve_ts_js("import b from '../b'", from, &map), vec![2]);
    }

    #[test]
    fn resolve_ts_js_esm_js_extension_maps_to_ts_sources() {
        let map = ids(&[
            ("src/a.ts", 1),
            ("src/b.tsx", 2),
            ("src/c.mts", 3),
            ("src/d.cts", 4),
            ("src/e.js", 5),
            ("src/e.ts", 6),
            ("src/x.ts", 7),
        ]);
        let from = Path::new("src/x.ts");
        assert_eq!(resolve_ts_js("import a from './a.js'", from, &map), vec![1]);
        assert_eq!(resolve_ts_js("import b from './b.js'", from, &map), vec![2]);
        assert_eq!(
            resolve_ts_js("import b from './b.jsx'", from, &map),
            vec![2]
        );
        assert_eq!(
            resolve_ts_js("import c from './c.mjs'", from, &map),
            vec![3]
        );
        assert_eq!(
            resolve_ts_js("import d from './d.cjs'", from, &map),
            vec![4]
        );
        // an existing .js file wins over the .ts counterpart
        assert_eq!(resolve_ts_js("import e from './e.js'", from, &map), vec![5]);
        // no cross mapping (.cjs must not map to .mts)
        assert!(resolve_ts_js("import c from './c.cjs'", from, &map).is_empty());
        // `dir.ts` wins over `dir.js/index.ts`
        let map2 = ids(&[
            ("src/dir.ts", 8),
            ("src/dir.js/index.ts", 9),
            ("src/x.ts", 7),
        ]);
        assert_eq!(
            resolve_ts_js("import d from './dir.js'", from, &map2),
            vec![8]
        );
    }

    #[test]
    fn resolve_ts_js_appended_extension_priority_follows_exts_order() {
        let from = Path::new("src/x.ts");
        let exts = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
        for (i, ext) in exts.iter().enumerate() {
            // each of the 8 extensions resolves on its own
            let only = ids(&[(&format!("src/a.{ext}"), 10)]);
            assert_eq!(resolve_ts_js("import a from './a'", from, &only), vec![10]);
            // and it loses to every earlier one in EXTS
            for earlier in &exts[..i] {
                let both = ids(&[
                    (&format!("src/a.{ext}"), 10),
                    (&format!("src/a.{earlier}"), 20),
                ]);
                assert_eq!(
                    resolve_ts_js("import a from './a'", from, &both),
                    vec![20],
                    "{earlier} should beat {ext}"
                );
            }
        }
    }

    #[test]
    fn resolve_ts_js_file_beats_directory_index_and_index_follows_exts_order() {
        let from = Path::new("src/x.ts");
        // `./a` -> a.ts before a/index.ts (even a.cjs beats a/index.ts)
        let map = ids(&[("src/a/index.ts", 2), ("src/a.ts", 1)]);
        assert_eq!(resolve_ts_js("import a from './a'", from, &map), vec![1]);
        let map = ids(&[("src/a/index.ts", 2), ("src/a.cjs", 3)]);
        assert_eq!(resolve_ts_js("import a from './a'", from, &map), vec![3]);
        // index.* priority follows EXTS order
        let exts = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
        for (i, ext) in exts.iter().enumerate() {
            let only = ids(&[(&format!("src/d/index.{ext}"), 10)]);
            assert_eq!(resolve_ts_js("import d from './d'", from, &only), vec![10]);
            for earlier in &exts[..i] {
                let both = ids(&[
                    (&format!("src/d/index.{ext}"), 10),
                    (&format!("src/d/index.{earlier}"), 20),
                ]);
                assert_eq!(
                    resolve_ts_js("import d from './d'", from, &both),
                    vec![20],
                    "index.{earlier} should beat index.{ext}"
                );
            }
        }
    }

    #[test]
    fn normalize_path_edge_cases() {
        assert_eq!(normalize_path(Path::new("./")), Some(PathBuf::new()));
        assert_eq!(normalize_path(Path::new(".")), Some(PathBuf::new()));
        assert_eq!(normalize_path(Path::new("")), Some(PathBuf::new()));
        assert_eq!(normalize_path(Path::new("./a")), Some(PathBuf::from("a")));
        // excess `..` above the root is reported as an escape (None), not swallowed
        assert_eq!(normalize_path(Path::new("a/../../b")), None);
        assert_eq!(normalize_path(Path::new("a/..")), Some(PathBuf::new()));
    }

    #[test]
    fn resolve_ruby_require_relative_and_require() {
        let map = ids(&[
            ("lib/a/main.rb", 1),
            ("lib/a/helper.rb", 2),
            ("lib/util.rb", 3),
            ("lib/x/y.rb", 4),
            ("top.rb", 5),
        ]);
        let from = Path::new("lib/a/main.rb");
        let r = |s: &str| resolve_ruby(s, from, &map);
        assert_eq!(r("require_relative 'helper'"), vec![2]);
        assert_eq!(r("require_relative \"./helper.rb\""), vec![2]);
        assert_eq!(r("require_relative('../util')"), vec![3]);
        assert_eq!(r("require 'util'"), vec![3]);
        assert_eq!(r("require \"x/y\""), vec![4]);
        assert_eq!(r("require 'top'"), vec![5]);
        // gems, missing files and repo-escaping paths are not resolved
        assert!(r("require 'json'").is_empty());
        assert!(r("require_relative 'missing'").is_empty());
        assert!(r("require_relative '../../../top'").is_empty());
        assert!(r("require '../top'").is_empty());
        assert!(r("require_relative").is_empty());
    }

    #[test]
    fn resolve_import_dispatches_ruby() {
        let map = ids(&[("lib/a.rb", 1), ("lib/b.rb", 2)]);
        assert_eq!(
            resolve_import(
                "require_relative 'b'",
                Path::new("lib/a.rb"),
                &map,
                &HashMap::new(),
                &[]
            ),
            vec![2]
        );
    }

    #[test]
    fn resolve_import_dispatches_by_extension() {
        let map = ids(&[("src/a.rs", 1)]);
        let go = HashMap::new();
        assert_eq!(
            resolve_import("crate::a", Path::new("src/main.rs"), &map, &go, &[]),
            vec![1]
        );
        assert!(resolve_import("x", Path::new("README.md"), &map, &go, &[]).is_empty());
    }

    /// #231: a failing COMMIT (deferred FK violation) must not leave the
    /// transaction open on the connection.
    #[test]
    fn build_deps_rolls_back_when_commit_fails() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE files (id INTEGER PRIMARY KEY);
             CREATE TABLE symbols (file_id INTEGER, name TEXT, kind TEXT);
             CREATE TABLE deps (from_file INTEGER, to_file INTEGER
                                  REFERENCES files(id) DEFERRABLE INITIALLY DEFERRED,
                                PRIMARY KEY (from_file, to_file));
             INSERT INTO symbols VALUES (1, 'crate::b', 'import');",
        )
        .unwrap();
        let paths: HashMap<i64, PathBuf> = [
            (1, PathBuf::from("src/a.rs")),
            (2, PathBuf::from("src/b.rs")),
        ]
        .into();
        assert!(build_deps(&conn, &paths, &[]).is_err());
        assert!(
            conn.is_autocommit(),
            "transaction left open after failed COMMIT"
        );
    }

    #[test]
    fn build_deps_inserts_edges_and_skips_self_and_unknown_files() {
        let conn = mem_db();
        conn.execute_batch(
            "INSERT INTO symbols VALUES (1, 'crate::b', 'import');
             INSERT INTO symbols VALUES (1, 'crate::a', 'import');
             INSERT INTO symbols VALUES (9, 'crate::b', 'import');
             INSERT INTO symbols VALUES (1, 'foo', 'function');
             INSERT INTO deps VALUES (7, 8);",
        )
        .unwrap();
        let paths: HashMap<i64, PathBuf> = [
            (1, PathBuf::from("src/a.rs")),
            (2, PathBuf::from("src/b.rs")),
        ]
        .into();
        build_deps(&conn, &paths, &[]).unwrap();

        let edges: Vec<(i64, i64)> = conn
            .prepare("SELECT from_file, to_file FROM deps")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // old row cleared, self-edge (a→a) and unknown file 9 skipped
        assert_eq!(edges, vec![(1, 2)]);
    }

    #[test]
    fn bfs_expand_decays_score_and_respects_depth() {
        let conn = mem_db();
        conn.execute_batch("INSERT INTO deps VALUES (1,2); INSERT INTO deps VALUES (3,2); INSERT INTO deps VALUES (3,4);")
            .unwrap();

        let d1 = bfs_expand(&conn, &[(1, 1.0)], 1).unwrap();
        assert_eq!(d1.len(), 2);
        assert_eq!(d1[&2], 0.5);

        let d2 = bfs_expand(&conn, &[(1, 1.0)], 2).unwrap();
        assert_eq!(d2[&3], 0.25);
        assert!(!d2.contains_key(&4));

        let d0 = bfs_expand(&conn, &[(1, 0.3), (1, 0.9)], 0).unwrap();
        assert_eq!(d0[&1], 0.9);
        assert!(bfs_expand(&conn, &[], 2).unwrap().is_empty());
    }

    fn insert_edges(conn: &Connection, edges: &[(i64, i64)]) {
        for (a, b) in edges {
            conn.execute("INSERT INTO deps VALUES (?1, ?2)", [a, b])
                .unwrap();
        }
    }

    #[test]
    fn bfs_expand_depth_zero_returns_only_seeds_even_with_edges() {
        let conn = mem_db();
        insert_edges(&conn, &[(1, 2), (3, 1)]);
        let r = bfs_expand(&conn, &[(1, 0.8)], 0).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[&1], 0.8);
    }

    #[test]
    fn bfs_expand_empty_seeds_with_edges_is_empty() {
        let conn = mem_db();
        insert_edges(&conn, &[(1, 2)]);
        assert!(bfs_expand(&conn, &[], 3).unwrap().is_empty());
    }

    #[test]
    fn bfs_expand_stops_when_frontier_exhausted() {
        let conn = mem_db();
        insert_edges(&conn, &[(1, 2)]);
        // Isolated seed: nothing to expand even at a large depth.
        let r = bfs_expand(&conn, &[(7, 1.0)], 10).unwrap();
        assert_eq!(r.len(), 1);
        // Graph exhausted after one hop: depth 10 equals depth 1.
        let r = bfs_expand(&conn, &[(1, 1.0)], 10).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[&2], 0.5);
    }

    #[test]
    fn bfs_expand_decays_half_then_quarter_over_two_hops() {
        let conn = mem_db();
        // 1 -> 2 (outgoing), 3 -> 2 (incoming to 2), 3 is two hops from 1.
        insert_edges(&conn, &[(1, 2), (3, 2), (4, 3)]);
        let r = bfs_expand(&conn, &[(1, 1.0)], 2).unwrap();
        assert_eq!(r[&1], 1.0);
        assert_eq!(r[&2], 0.5);
        assert_eq!(r[&3], 0.25);
        assert!(!r.contains_key(&4));
    }

    #[test]
    fn bfs_expand_takes_max_over_multiple_parents() {
        let conn = mem_db();
        // 9 is reached from both seeds; the stronger parent wins in either order.
        insert_edges(&conn, &[(1, 9), (9, 2)]);
        let r = bfs_expand(&conn, &[(1, 1.0), (2, 0.4)], 1).unwrap();
        assert_eq!(r[&9], 0.5);
        let r = bfs_expand(&conn, &[(2, 0.4), (1, 1.0)], 1).unwrap();
        assert_eq!(r[&9], 0.5);
    }

    #[test]
    fn bfs_expand_duplicate_seeds_use_max_score_for_neighbours() {
        let conn = mem_db();
        insert_edges(&conn, &[(1, 2)]);
        for seeds in [[(1, 0.9), (1, 0.3)], [(1, 0.3), (1, 0.9)]] {
            let r = bfs_expand(&conn, &seeds, 1).unwrap();
            assert_eq!(r[&1], 0.9);
            assert_eq!(r[&2], 0.45);
        }
    }

    #[test]
    fn bfs_expand_keeps_better_score_found_at_shallower_depth() {
        let conn = mem_db();
        // 3 is reachable at depth 1 (0.5) and depth 2 (0.25): the larger one stays.
        insert_edges(&conn, &[(1, 2), (2, 3), (1, 3)]);
        let r = bfs_expand(&conn, &[(1, 1.0)], 2).unwrap();
        assert_eq!(r[&3], 0.5);
    }

    #[test]
    fn bfs_expand_decays_per_parent_not_from_frontier_max() {
        let conn = mem_db();
        // Non-seed neighbours must inherit their own parent's score (#135), not the
        // best score in the whole frontier.
        insert_edges(&conn, &[(1, 10), (2, 11)]);
        let r = bfs_expand(&conn, &[(1, 1.0), (2, 0.2)], 1).unwrap();
        assert_eq!(r[&10], 0.5);
        assert_eq!(r[&11], 0.1);
    }

    #[test]
    fn bfs_expand_terminates_on_cycles_and_never_raises_seed_score() {
        let conn = mem_db();
        insert_edges(&conn, &[(1, 2), (2, 1), (2, 3), (3, 1)]);
        let r = bfs_expand(&conn, &[(1, 1.0)], 50).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[&1], 1.0);
        assert_eq!(r[&2], 0.5);
        assert_eq!(r[&3], 0.5);
    }

    #[test]
    fn query_neighbors_batch_empty_ids_returns_empty() {
        let conn = mem_db();
        insert_edges(&conn, &[(1, 2)]);
        assert!(query_neighbors_batch(&conn, &[]).unwrap().is_empty());
    }

    #[test]
    fn query_neighbors_batch_returns_both_directions_and_excludes_inputs() {
        let conn = mem_db();
        insert_edges(&conn, &[(1, 2), (3, 1), (1, 4), (1, 5)]);
        let mut e = query_neighbors_batch(&conn, &[1, 4]).unwrap();
        e.sort();
        // (1,4) is internal to the input set and dropped.
        assert_eq!(e, vec![(1, 2), (1, 3), (1, 5)]);
    }

    #[test]
    fn query_neighbors_batch_handles_ids_over_sqlite_variable_limit() {
        let conn = mem_db();
        let n: i64 = 40_000; // > SQLite's 32766 bound-variable limit
        let tx = conn.unchecked_transaction().unwrap();
        {
            let mut stmt = conn
                .prepare("INSERT INTO deps (from_file, to_file) VALUES (?1, ?2)")
                .unwrap();
            for i in 1..=n {
                if i % 2 == 0 {
                    stmt.execute([i, 100_000 + i]).unwrap();
                } else {
                    stmt.execute([100_000 + i, i]).unwrap();
                }
            }
        }
        tx.commit().unwrap();
        let ids: Vec<i64> = (1..=n).collect();
        let e = query_neighbors_batch(&conn, &ids).unwrap();
        assert_eq!(e.len(), n as usize);
        assert!(e.contains(&(n, 100_000 + n)));
        assert!(e.contains(&(1, 100_001)));
    }

    #[test]
    fn bfs_expand_handles_frontier_over_1000_ids() {
        let conn = mem_db();
        let n: i64 = 1500;
        let tx = conn.unchecked_transaction().unwrap();
        for i in 1..=n {
            // Alternate directions so both IN queries return rows.
            if i % 2 == 0 {
                insert_edges(&conn, &[(i, 100_000 + i)]);
            } else {
                insert_edges(&conn, &[(100_000 + i, i)]);
            }
        }
        tx.commit().unwrap();
        let seeds: Vec<(i64, f32)> = (1..=n).map(|i| (i, 1.0)).collect();
        let r = bfs_expand(&conn, &seeds, 1).unwrap();
        assert_eq!(r.len(), 2 * n as usize);
        assert!(r.get(&(100_000 + n)).copied() == Some(0.5));
        assert_eq!(r[&(100_001)], 0.5);
    }
}
