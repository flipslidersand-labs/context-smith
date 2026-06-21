# Implementation Guide — ContextSmith

## Phase 1: Git リポジトリ走査（1週）

### 実装内容

- `src/git_walker.rs` — `git2::Repository` でファイル一覧を取得
- HEAD ツリーを walk して `.rs` / `.py` / `.go` のパスと blob SHA を収集
- `git2::Diff` で直近 N コミットの差分を取得

### 完成条件

```bash
contextsmith index --repo ./project
# Found 312 files (rust: 198, python: 87, other: 27)
# Recent diff: 14 files changed
```

---

## Phase 2: Tree-sitter シンボル抽出（1〜2週）

### 実装内容

- `src/symbol_extractor.rs` — `tree-sitter` で各ファイルを解析
- Rust: `fn`, `struct`, `impl`, `use` を抽出
- Python: `def`, `class`, `import` を抽出
- 抽出したシンボルを SQLite の `symbols` テーブルに保存

### 完成条件

```bash
contextsmith index --repo ./project
# Indexed 1,843 symbols (functions: 921, structs: 412, imports: 510)
```

### 難所

- Tree-sitter の S 式クエリ (`(function_item name: (identifier) @name)`) の書き方
- 大きなファイルの解析速度 → `tree-sitter` はインクリメンタル解析に対応

---

## Phase 3: 依存グラフ構築（1週）

### 実装内容

- `src/dep_builder.rs` — `use` / `import` 文を解析し `deps` テーブルに保存
- `petgraph::DiGraph<FileId, ()>` で依存グラフを構築
- タスク関連ファイルを起点に BFS で依存先を展開

### 完成条件

```bash
contextsmith build --task "認証エラー" --budget 10000 --explain
# src/auth/mod.rs (seed, BM25=0.82)
#   └─▶ src/auth/jwt.rs (dep of mod.rs)
#   └─▶ src/auth/session.rs (dep of mod.rs)
```

---

## Phase 4: BM25 全文検索（1週）

### 実装内容

- `src/indexer.rs` — `tantivy` でファイル内容 + シンボル名のインデックスを構築
- `src/retriever.rs` — タスク文字列で BM25 クエリを発行しスコア付きファイル一覧を取得

### 完成条件

```bash
contextsmith build --task "JWT 有効期限チェック" --budget 20000
# Top-5: jwt.rs(0.91), auth.rs(0.74), session.rs(0.61), middleware.rs(0.53), config.rs(0.44)
```

---

## Phase 5: 予算配分 + バンドル出力（1週）

### 実装内容

- `src/budget.rs` — スコア降順にファイルをトークン換算し `budget` に収まる分を選択
- `src/bundle_writer.rs` — `context.bundle/` ディレクトリに Markdown + `citations.json` を書き出す

### 完成条件

```bash
contextsmith build --task "認証エラー" --budget 30000 --out ./context.bundle/
# Used 28,412 / 30,000 tokens
# Files: 7, Symbols: 43, Diff lines: 128
```

---

## Phase 6: ベクトル検索 + RRF 統合（2週）

### 実装内容

- ローカル埋め込みモデル（`fastembed-rs`）でシンボルスニペットをベクトル化
- `hnswlib` または `usearch` でベクトルインデックスを構築
- BM25 スコアとベクトルスコアを RRF (Reciprocal Rank Fusion) で統合

### 完成条件

```bash
contextsmith build --task "ログイン後に session が null になる" --budget 30000
# BM25 単独より関連度が高いファイルが上位に来ることを評価セットで確認
```

---

## 実装順序の根拠

Git 走査 → シンボル抽出 → 依存グラフ → BM25 の順は「データ収集 → 構造化 → 検索」の自然な流れ。
Phase 6 のベクトル検索は BM25 の精度限界が見えてから追加することで、
「本当に必要か」を数値で判断できる。
