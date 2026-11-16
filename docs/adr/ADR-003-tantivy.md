# ADR-003: BM25 検索に SQLite FTS5 を使う

- **日付**: 2026-06-20
- **更新**: 2026-11-15（実装照合を追記。CHANGELOG 0.4.1 時点の実装に基づく）
- **状態**: Accepted（tantivy から FTS5 に変更）

## 背景

BM25 全文検索の実装方法として、SQLite FTS5・Tantivy・Meilisearch・Elasticsearch の選択肢がある。
当初 Tantivy を採用していたが、設計レビューで前提が変わったため再決定した。

## 決定

`rusqlite` に内包されている **SQLite FTS5** を使う。フィールド重み付けは複数の FTS5 テーブルを UNION ALL で合算して実現する。

> **注意**: 以下の 2 テーブル SQL（symbols 重み 2 倍・`ORDER BY total ASC`）は**決定時の設計例**で、現行の検索 SQL ではない。現行は「実装照合」節を参照。

### 決定時の設計例（現行ではない）

```sql
CREATE VIRTUAL TABLE fts_symbols USING fts5(file_id UNINDEXED, name);
-- シンボル名を pre-tokenize して格納（重み 2 倍）

CREATE VIRTUAL TABLE fts_body USING fts5(file_id UNINDEXED, content);
-- オリジナルのファイル内容をそのまま格納（snippet() 用、重み 1 倍）
```

```sql
SELECT file_id, SUM(score) AS total
FROM (
    SELECT file_id, rank * 2.0 AS score FROM fts_symbols WHERE name    MATCH ?1
    UNION ALL
    SELECT file_id, rank       AS score FROM fts_body    WHERE content MATCH ?1
)
GROUP BY file_id ORDER BY total ASC LIMIT 20
```

## 理由

- **追加依存ゼロ**: `rusqlite = { features = ["bundled"] }` に FTS5 が内包されており、クレート追加不要
- **単一ファイル**: インデックスが `index.db` 1 ファイルに収まる（Tantivy はディレクトリ必須）
- **用途適合**: context-smith は単一リポジトリを対象とするため、Tantivy の大規模向け機能は不要
- **フィールド重み付け**: 複数テーブル + UNION ALL + GROUP BY で実現できることを実測で確認（決定時は 2 テーブル、現行は 3 テーブル）
- **git 管理可能**: 小規模リポジトリなら `index.db` をそのまま commit して共有できる
- **snippet() を将来使える**: fts_body にオリジナル内容を格納しているため、必要になれば FTS5 組み込みの `snippet()` で抽出できる（現行の `src` では呼び出していない。シンボルのスニペットは index 構築時に `symbols.snippet` へ保存している）

## Tantivy を選ばなかった理由

| 懸念                             | 実態                                     |
| -------------------------------- | ---------------------------------------- |
| Tantivy のフィールド重み付け優位 | 複数テーブル方式（現行は 3 テーブル）で FTS5 でも実現可能 |
| 大規模での精度差                 | 対象が単一リポジトリなので問題にならない |
| +127 クレートのビルドコスト      | FTS5 なら不要                            |
| ディレクトリ管理・git 管理       | FTS5 なら index.db 1 ファイルで完結      |

## トレードオフ

- camelCase（`authenticateUser` → `authenticate user`）は FTS5 の tokenizer が分割できないため、
  アプリ側でプリトークナイズして格納する（`src/tokenizer.rs`）
- Phase 6 でベクトル検索と RRF 統合する際は、現行の BM25 スコアは、テーブル別スコアを `rank / MIN(rank)` で 0〜1 に正規化し、重み付き合算した total（大きいほど良い）である点を踏まえる。重み付け後の total は 0〜1 に収まらない。生の FTS5 rank（負値）を再度変換すると二重変換になるため、`search_bm25` の返すスコアまたは順位をそのまま RRF の入力にする

## 実装照合（2026-11-15、CHANGELOG 0.4.1 時点）

現在は `fts_path`・`fts_symbols`・`fts_body` の 3 テーブルを使用する（定義は [index_builder.rs](../../src/index_builder.rs)）。検索は [search_index.rs](../../src/search_index.rs) にあり、次のとおり。

- 各テーブルの BM25 `rank` を、そのクエリ内の `MIN(rank)` で割って 0〜1 に正規化する（`NULLIF(..., 0)` でゼロ除算を避け、`COALESCE(SUM(score), 0)` で NULL を 0 にする）。
- 重みは `W_PATH`・`W_SYMBOLS`・`W_BODY` の定数で、値と根拠は `search_index.rs` 冒頭のコメントを参照する。symbols は当初 2.0 だったが、シンボル数の多いファイルが最良一致のファイルを上回らないよう、file 単位の best match 集約と合わせて 0.5 に下げた（#151、CHANGELOG 0.4.1）。#135 は再現率を計測した Issue である。
- `fts_symbols` は 1 シンボル 1 行なので、file 単位に `MIN(rank)` で畳んでから合算する（多シンボルのファイルが過大評価されるのを防ぐ）。
- 各サブクエリは上位だけ取ってから UNION ALL し、total の降順で `top_n` 件を返す（候補数の係数は `search_bm25` のドキュメントコメントを参照）。

上記の 2 テーブル SQL は当時の設計例である。SQLite FTS5 の採用自体は変更しない。
