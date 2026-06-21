# Data Model — ContextSmith

## シンボル DB (SQLite)

```sql
CREATE TABLE files (
    id       INTEGER PRIMARY KEY,
    path     TEXT NOT NULL UNIQUE,
    lang     TEXT NOT NULL,          -- "rust" | "python" | "go"
    blob_sha TEXT NOT NULL,
    size     INTEGER NOT NULL
);

CREATE TABLE symbols (
    id       INTEGER PRIMARY KEY,
    file_id  INTEGER NOT NULL REFERENCES files(id),
    name     TEXT NOT NULL,
    kind     TEXT NOT NULL,          -- "function" | "struct" | "class" | "import"
    line     INTEGER NOT NULL,
    col      INTEGER NOT NULL,
    snippet  TEXT NOT NULL           -- 前後5行
);

CREATE TABLE deps (
    from_file INTEGER NOT NULL REFERENCES files(id),
    to_file   INTEGER NOT NULL REFERENCES files(id),
    PRIMARY KEY (from_file, to_file)
);
```

## Rust 構造体

```rust
pub struct FileInfo {
    pub id:       i64,
    pub path:     PathBuf,
    pub lang:     Language,
    pub blob_sha: String,
}

pub struct Symbol {
    pub file_id: i64,
    pub name:    String,
    pub kind:    SymbolKind,
    pub line:    u32,
    pub snippet: String,
}

#[derive(Clone, Copy)]
pub enum SymbolKind { Function, Struct, Class, Impl, Import }

pub enum Language { Rust, Python, Go, Unknown }
```

## バンドル出力

```rust
pub struct ContextBundle {
    pub task:       String,
    pub budget:     usize,              // トークン上限
    pub used:       usize,              // 実際に使用したトークン数
    pub files:      Vec<BundleFile>,
    pub citations:  Vec<Citation>,
    pub diff_summary: String,
}

pub struct BundleFile {
    pub path:    PathBuf,
    pub content: String,                // 抜粋（全体 or 関連スニペット）
    pub score:   f64,                   // BM25 スコア
    pub tokens:  usize,
}

pub struct Citation {
    pub path:   PathBuf,
    pub line:   u32,
    pub symbol: String,
}
```

## スコアリングパイプライン

```
タスク文字列
  → [Tantivy] BM25 スコア × ファイル
  → [GraphExpander] 上位 N ファイルの依存 BFS → 追加候補
  → [BudgetAllocator] スコア降順に tokens を積み上げ、budget 超えたら打ち切り
  → [BundleWriter] Markdown + citations.json
```
