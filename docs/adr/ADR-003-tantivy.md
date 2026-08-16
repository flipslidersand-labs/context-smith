# ADR-003: BM25 検索に SQLite FTS5 を使う

- **日付**: 2026-06-20
- **更新**: 2026-08-16
- **状態**: Accepted（tantivy から FTS5 に変更）

## 背景

BM25 全文検索の実装方法として、SQLite FTS5・Tantivy・Meilisearch・Elasticsearch の選択肢がある。
当初 Tantivy を採用していたが、設計レビューで前提が変わったため再決定した。

## 決定

`rusqlite` に内包されている **SQLite FTS5** を使う。2テーブル構成でフィールド重み付けを実現する。

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
- **フィールド重み付け**: 2テーブル + UNION ALL + GROUP BY で実現できることを実測で確認
- **git 管理可能**: 小規模リポジトリなら `index.db` をそのまま commit して共有できる
- **snippet() 組み込み**: fts_body にオリジナル内容を格納することで SQL 1 行でスニペット抽出可能

## Tantivy を選ばなかった理由

| 懸念                             | 実態                                     |
| -------------------------------- | ---------------------------------------- |
| Tantivy のフィールド重み付け優位 | 2テーブル方式で FTS5 でも実現可能        |
| 大規模での精度差                 | 対象が単一リポジトリなので問題にならない |
| +127 クレートのビルドコスト      | FTS5 なら不要                            |
| ディレクトリ管理・git 管理       | FTS5 なら index.db 1 ファイルで完結      |

## トレードオフ

- camelCase（`authenticateUser` → `authenticate user`）は FTS5 の tokenizer が分割できないため、
  アプリ側でプリトークナイズして格納する（`src/tokenizer.rs`）
- Phase 6 でベクトル検索と RRF 統合する際は FTS5 の rank（負値）を正値に変換してから合算する
