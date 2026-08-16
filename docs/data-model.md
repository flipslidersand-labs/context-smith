# Data Model — ContextSmith

## インデックス DB (SQLite: `.contextsmith/index.db`)

すべてのテーブルを 1 ファイルに統合する。小規模リポジトリなら git 管理対象にして共有可能。

```sql
CREATE TABLE files (
    id       INTEGER PRIMARY KEY,
    path     TEXT NOT NULL UNIQUE,
    lang     TEXT NOT NULL,          -- "rust" | "python" | "go" | "unknown"
    blob_sha TEXT NOT NULL,
    size     INTEGER NOT NULL
);

CREATE TABLE symbols (
    id      INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id),
    name    TEXT NOT NULL,
    kind    TEXT NOT NULL,           -- "function" | "struct" | "class" | "impl" | "import"
    line    INTEGER NOT NULL,
    snippet TEXT NOT NULL            -- 前後 5 行
);

CREATE TABLE deps (
    from_file INTEGER NOT NULL REFERENCES files(id),
    to_file   INTEGER NOT NULL REFERENCES files(id),
    PRIMARY KEY (from_file, to_file)
);

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
    -- repo_root: canonicalize 済みの絶対パス
    -- indexed_at: ISO 8601 タイムスタンプ
);

-- BM25 検索（シンボル名・重み 2 倍）
-- name には tokenize_code() でプリトークナイズした文字列を格納
CREATE VIRTUAL TABLE fts_symbols USING fts5(file_id UNINDEXED, name);

-- BM25 検索（ファイル本文・重み 1 倍）
-- content にはオリジナルのファイル内容をそのまま格納（snippet() 用）
CREATE VIRTUAL TABLE fts_body USING fts5(file_id UNINDEXED, content);
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
pub enum SelectionReason {
    Bm25 { rank: usize, score: f32 },
    DepExpansion { parent: PathBuf, depth: u8 },
}

pub struct BundleFile {
    pub path:    PathBuf,
    pub content: String,   // FTS5 snippet() または先頭 N 行
    pub tokens:  usize,    // content.chars().count() / 4
    pub reason:  SelectionReason,
}

pub struct ContextBundle {
    pub task:         String,
    pub budget:       usize,
    pub used:         usize,
    pub files:        Vec<BundleFile>,
    pub diff_summary: String,
}

pub struct Citation {
    pub path:   PathBuf,
    pub line:   u32,
    pub symbol: String,
    pub score:  f32,
}
```

## スコアリングパイプライン

```
タスク文字列
  → [FTS5 2テーブル BM25] シンボル名（×2）+ 本文（×1）をスコア合算
      SELECT file_id, SUM(score) FROM (
          SELECT file_id, rank * 2.0 FROM fts_symbols WHERE name    MATCH ?
          UNION ALL
          SELECT file_id, rank       FROM fts_body    WHERE content MATCH ?
      ) GROUP BY file_id ORDER BY total ASC
  → [DepBuilder BFS] 上位 N ファイルを seed に双方向 BFS（depth=2）
      スコア減衰: parent_score × 0.5^depth
  → [BudgetAllocator] スコア降順に tokens を積み上げ、budget 超えたら打ち切り
  → [BundleWriter] Markdown + citations.json
```
