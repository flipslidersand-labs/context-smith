# Tech Stack — ContextSmith

## 言語・バージョン

- Rust 1.89+ (edition 2021)。`Cargo.toml` の `rust-version` が正本（rusqlite 0.41 の要求に合わせた値）

## 主要クレートと選定理由

版は `Cargo.toml` が正本。この表は 0.5.0 時点。

| クレート               | バージョン | 役割                                         | 選定理由                                        |
| ---------------------- | ---------- | -------------------------------------------- | ----------------------------------------------- |
| `git2`                 | 0.21       | Git リポジトリ走査・差分取得                 | libgit2 の Rust バインディング、GitHub API 不要 |
| `tree-sitter`          | 0.26       | シンボル抽出・AST 解析                       | 多言語対応・インクリメンタル解析が強力          |
| `tree-sitter-*` 文法   | 0.23〜0.25 | Rust / Python / Go / TypeScript / JavaScript / Ruby | tree-sitter の言語グラマー。javascript のみ 0.25（0.26 ランタイムが前提） |
| `rusqlite`             | 0.41       | シンボル・インデックスの永続化、FTS5 による BM25 検索 | ファイル1つで完結。検索も同じ DB に置ける（ADR-003） |
| `serde` / `serde_json` | 1          | citations.json・`query --format json` 出力   | derive で簡潔                                   |
| `clap`                 | 4          | CLI                                          | derive マクロ                                   |
| `anyhow`               | 1          | エラーハンドリング                           | 複数クレートのエラーを集約                      |
| `ureq` / `zeroize`     | 2 / 1      | 埋め込みサービス連携（`remote-embed` feature のみ） | 既定ビルドは BM25 のみでオフライン動作      |

SQLite は既定で同梱ビルド（`bundled-sqlite` feature）。`--no-default-features` でシステムの SQLite を使う。

## アーキテクチャ

```
contextsmith index
  ├── index_builder (git2)           HEAD の全ファイルを走査
  ├── tree-sitter                    関数・型・import を抽出
  ├── dep_builder                    import 関係を deps テーブルに保存
  └── search_index (SQLite FTS5)     シンボル・本文・パスの検索インデックス

contextsmith build --task "..." --budget N
  ├── select (search_index)          FTS5 の BM25 でタスク関連ファイルをランキング
  ├── select (deps テーブル)         上位ファイルの依存を BFS 展開
  ├── budget                         トークン数を計算しグリーディー選択
  └── bundle_writer                  Markdown + citations.json 出力
```

## 開発ツール

| ツール               | 用途                                                 |
| -------------------- | ---------------------------------------------------- |
| `clippy` / `rustfmt` | linting / フォーマット                               |
| `cargo-flamegraph`   | 大規模リポジトリでのインデックス速度プロファイリング |
