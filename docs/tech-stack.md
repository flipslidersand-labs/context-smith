# Tech Stack — ContextSmith

## 言語・バージョン

- Rust 1.78+ (edition 2021)

## 主要クレートと選定理由

| クレート               | バージョン | 役割                           | 選定理由                                        |
| ---------------------- | ---------- | ------------------------------ | ----------------------------------------------- |
| `git2`                 | 0.19       | Git リポジトリ走査・差分取得   | libgit2 の Rust バインディング、GitHub API 不要 |
| `tree-sitter`          | 0.22       | シンボル抽出・AST 解析         | 多言語対応・インクリメンタル解析が強力          |
| `tree-sitter-rust`     | 0.21       | Rust 文法                      | tree-sitter の言語グラマー                      |
| `tree-sitter-python`   | 0.21       | Python 文法                    | 同上                                            |
| `tantivy`              | 0.21       | BM25 全文検索インデックス      | Rust ネイティブ・Lucene 相当の機能              |
| `petgraph`             | 0.6        | 依存グラフ + BFS/DFS           | R2 DeltaForge と共通パターン                    |
| `rusqlite`             | 0.31       | シンボル・インデックスの永続化 | ファイル1つで完結                               |
| `serde` / `serde_json` | 1          | citations.json・グラフ出力     | derive で簡潔                                   |
| `clap`                 | 4          | CLI                            | derive マクロ                                   |
| `anyhow`               | 1          | エラーハンドリング             | 複数クレートのエラーを集約                      |

## アーキテクチャ

```
contextsmith index
  ├── GitWalker (git2)       全ファイルの blob を取得
  ├── SymbolExtractor (tree-sitter)  関数・型・import を抽出
  ├── DependencyBuilder (petgraph)   import 関係を有向グラフ化
  └── Indexer (tantivy + rusqlite)   BM25 インデックス + シンボル DB 構築

contextsmith build --task "..." --budget N
  ├── Retriever (tantivy)    BM25 でタスク関連ファイルをランキング
  ├── GraphExpander (petgraph) 上位ファイルの依存を BFS 展開
  ├── BudgetAllocator        トークン数を計算しグリーディー選択
  └── BundleWriter           Markdown + citations.json 出力
```

## 開発ツール

| ツール               | 用途                                                 |
| -------------------- | ---------------------------------------------------- |
| `clippy` / `rustfmt` | linting / フォーマット                               |
| `cargo-flamegraph`   | 大規模リポジトリでのインデックス速度プロファイリング |
