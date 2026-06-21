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
# インデックス構築
contextsmith index --repo ./project

# コンテキスト生成
contextsmith build \
  --repo ./project \
  --task "認証エラーを調査" \
  --budget 30000 \
  --out context.bundle/
```

## 出力バンドル

```
context.bundle/
├── task.md               # タスク説明 + 選択理由
├── relevant-code/        # 関連ファイル抜粋
├── dependency-graph.json # シンボル依存グラフ
├── recent-diffs/         # 直近の Git 差分
├── schema-summary.md     # DB スキーマ要約 (将来)
├── related-issues.md     # 関連 Issue (将来)
└── citations.json        # 各スニペットのファイル・行番号参照
```

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
| Phase 3 | 依存グラフ (petgraph) を構築しタスク起点で BFS 展開             |
| Phase 4 | BM25 スコアリングでファイルをランキング                         |
| Phase 5 | `--budget N` 内に収めるグリーディー選択 + Markdown バンドル出力 |
| Phase 6 | ローカル埋め込みモデルでベクトル検索を追加し BM25 と RRF で統合 |
