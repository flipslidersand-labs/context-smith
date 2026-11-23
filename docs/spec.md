# Spec — ContextSmith

## プロジェクトの目的

Git リポジトリ・Issue・設計書・DB スキーマ・ログから、タスクに必要な情報だけを選び出し、
トークン予算内に収まる AI 向けコンテキストバンドルを生成するコンパイラ。

## 解決する問題

| 問題                                           | ContextSmith での解決策                            |
| ---------------------------------------------- | -------------------------------------------------- |
| 大規模リポジトリの全コードを AI に渡せない     | タスク関連度スコアで上位ファイルだけを選択         |
| 関係ないコードが多いと AI の精度が落ちる       | 依存グラフ + BM25 でタスクに直結するシンボルを抽出 |
| 「どこを変えたか」を毎回手動で伝える必要がある | Git 差分を自動取得してバンドルに含める             |

## 利用イメージ

```bash
# インデックス構築（.contextsmith/index.db をリポジトリ内に生成）
contextsmith index --repo ./project

# コンテキスト生成（.contextsmith/index.db を自動参照）
contextsmith build \
  --repo ./project \
  --task "認証エラーを調査" \
  --budget 30000 \
  --out context.bundle/

# ファイルへ書かずに stdout へ出す(スクリプト・パイプ向け。--format json|md)
contextsmith query --repo ./project --task "認証エラーを調査" --format json
```

## インデックスの格納場所

`contextsmith index` は `{repo}/.contextsmith/index.db` に SQLite ファイルを生成する。
このファイルには files / symbols / deps / meta / fts_path / fts_symbols / fts_body がすべて含まれる(テーブルの定義は [data-model.md](./data-model.md))。

小規模リポジトリ（目安: ソースコード 10MB 以下）であれば `index.db` を git 管理対象にして
チームで共有できる。大規模リポジトリの場合は `.gitignore` に追加することを推奨する。

## 出力バンドル

```
context.bundle/
├── task.md               # タスク説明 + 選択したファイル一覧 + 直近の Git 差分(`## Recent Diff`)
├── relevant-code/        # 選択したファイルの内容(ファイルごとに 1 つの .md)
└── citations.json        # 各ファイルのスコアとトークン数。末尾に `(recent diff)` の項目
```

`dependency-graph.json`・`recent-diffs/`・`schema-summary.md`・`related-issues.md` は**未実装**(出力されない)。
直近の差分は `task.md` に含まれる。

## MVP の境界線

### やること (Phase 1〜5)

- Git リポジトリ走査（`git2` でファイル一覧・差分取得）
- Tree-sitter でシンボル抽出（関数・クラス・型の名前と位置）
- import / use 依存解析で依存グラフ構築
- BM25 キーワードランキングでタスク関連ファイルを絞り込み
- トークン予算（`--budget N`）内に収めるグリーディー選択
- Markdown バンドル出力 + `citations.json`

### やらないこと (Phase 1)

- ベクトル検索（埋め込みモデル）
- Issue / PR 連携
- DB スキーマ解析
- MCP サーバー化
- 増分インデックス

## 成功条件

| Phase   | 完成条件                                                        |
| ------- | --------------------------------------------------------------- |
| Phase 1 | `git2` でリポジトリを走査しファイル一覧と HEAD 差分を取得       |
| Phase 2 | Tree-sitter で Rust / Python のシンボルを抽出し SQLite に保存   |
| Phase 3 | 依存グラフ (SQLite の deps テーブル) を構築しタスク起点で BFS 展開  |
| Phase 4 | BM25 スコアリングでファイルをランキング                         |
| Phase 5 | `--budget N` 内に収めるグリーディー選択 + Markdown バンドル出力 |
| Phase 6 | 埋め込みシードを追加し BM25 と RRF で統合(opt-in の `remote-embed` feature・リモート埋め込みサービス経由。ローカル埋め込みモデルは未実装) |
