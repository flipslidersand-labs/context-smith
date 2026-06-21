# ADR-003: BM25 インデックスに Tantivy を使う

- **日付**: 2026-06-20
- **状態**: Accepted

## 背景

BM25 全文検索の実装方法として、自前実装・Tantivy・Meilisearch・Elasticsearch の選択肢がある。

## 決定

`tantivy` を使う。

## 理由

- Rust ネイティブで外部プロセス不要（Meilisearch / Elasticsearch はサーバーが必要）
- `search-engine (ACTIVE)` プロジェクトで SQLite FTS5 を実装済みなので BM25 の概念は既習
- Tantivy は Lucene 相当の機能（スコアリング・フィールド重み付け・インクリメンタル更新）を持つ
- `cargo build` 1 コマンドで動くためデプロイが簡単

## トレードオフ

- API が Lucene 由来で最初は学習コストがある（`IndexWriter`・`Searcher`・`Query` の関係）
