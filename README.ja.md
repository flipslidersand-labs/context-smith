# context-smith

**English**: [README.md](./README.md) | **日本語**: このページ

**AI context compiler** — Git リポジトリのファイルをタスクの説明に照らして順位づけし(BM25 + 依存グラフ)、トークン予算内に収まるコンテキストバンドルを生成する CLI ツール。

```
大規模リポジトリ全体をそのまま渡す → トークンが多く、大半はタスクと無関係
              ↓
contextsmith が BM25 + 依存グラフ でファイルを順位づけ
              ↓
トークン予算に収まるバンドルを出力 → リポジトリ全体よりトークンがずっと少ない
```

バンドルが AI の回答を良くするかは**測っていません**。順位づけが正しいファイルを見つける精度は、下の「精度」に示します。

## インストール

```bash
# crates.io から（推奨）
cargo install context-smith

# ソースから
cargo install --path .
```

依存ライブラリ（`bundled` フィーチャーで自動ビルド）: libgit2, SQLite, tree-sitter

## 使い方

### 1. インデックス構築

```bash
contextsmith index --repo ./my-project
```

`./my-project/.contextsmith/index.db` に SQLite インデックスを生成する。
ファイル一覧・シンボル・依存関係・BM25 全文インデックスがすべて 1 ファイルに収まる。

小規模リポジトリ（目安: ソース 10MB 以下）なら `index.db` を git 管理してチーム共有できる。

| オプション | デフォルト | 説明 |
|---|---|---|
| `--repo <PATH>` | 必須 | 対象リポジトリのパス |
| `--out <PATH>` | `.contextsmith/index.db` | 出力先の上書き |

### 2. コンテキストバンドル生成

```bash
contextsmith build \
  --repo ./my-project \
  --task "認証エラーを調査したい" \
  --budget 30000
```

`context.bundle/` ディレクトリにバンドルを出力する。

| オプション | デフォルト | 説明 |
|---|---|---|
| `--repo <PATH>` | 必須 | 対象リポジトリのパス |
| `--task <TEXT>` | 必須 | タスクの説明 |
| `--budget <N>` | `30000` | トークン上限 |
| `--out <PATH>` | `context.bundle` | 出力ディレクトリ |
| `--explain` | off | task.md に BM25 スコアを表示 |
| `--diff-commits <N>` | `3` | 差分に含める直近コミット数 |

### バンドルの構造

```
context.bundle/
├── task.md           # タスク説明 + 選択ファイル一覧 + 直近 Git 差分
├── relevant-code/    # 選択されたファイルの内容（Markdown コードブロック）
└── citations.json    # ファイルごとのスコア・トークン数（機械可読）
```

### 3. クエリ（stdout・スクリプト/パイプ向け）

`query` は `build` と同じ選択パイプラインを実行するが、ディスクに書き込まず
stdout に出力する。CI やシェルスクリプトからの利用に適する。

```bash
# 機械可読なファイル選択結果
contextsmith query --repo . --task "認証エラー" --format json | jq '.files[].path'

# task.md 相当のサマリー
contextsmith query --repo . --task "認証エラー" --format md --explain
```

| オプション | デフォルト | 説明 |
|---|---|---|
| `--repo <PATH>` | 必須 | 対象リポジトリのパス |
| `--task <TEXT>` | 必須 | タスクの説明 |
| `--budget <N>` | `30000` | トークン上限 |
| `--format <json\|md>` | `json` | 出力フォーマット |
| `--explain` | off | BM25 スコア表示（md のみ） |
| `--index <PATH>` | `{repo}/.contextsmith/index.db` | インデックスパス上書き |

JSON 出力は `{ task, budget, used_tokens, files: [{ path, score, tokens }] }` 形式。

## アーキテクチャ

```
contextsmith index          contextsmith build
      │                           │
      ▼                           ▼
 [GitRepo.scan()]         [search_bm25()]       ← FTS5 UNION ALL
 [SymbolExtractor]        [bfs_expand()]        ← petgraph BFS
 [build_deps()]           [allocate()]          ← greedy budget
 [populate_fts()]         [write_bundle()]      ← Markdown + JSON
      │
      ▼
 .contextsmith/index.db (SQLite)
 ├── files        — パス・言語・blob SHA
 ├── symbols      — 関数/構造体/クラス名・行番号・スニペット
 ├── deps         — ファイル間の import 依存グラフ
 ├── meta         — repo_root・indexed_at
 ├── fts_path     — FTS5 (トークン化したファイルパス、BM25 weight ×3)
 ├── fts_symbols  — FTS5 (pre-tokenized、BM25 weight ×2)
 └── fts_body     — FTS5 (ファイル本文、BM25 weight ×1)
```

### スコアリングパイプライン

1. **BM25 検索** — タスク文字列で `fts_path`（重み×3）・`fts_symbols`（重み×2）・`fts_body`（重み×1）を UNION ALL で検索、上位 20 件を seed として取得。タスクが 9 語以上(重複除く)のときは、先に出現ファイル数が少ない順の上位 8 語に絞る(一般語が固有の語を埋もれさせないため)
2. **依存グラフ BFS** — seed ファイルから bidirectional BFS（depth=2）でスコアを伝播（hop ごとに ×0.5 減衰）
3. **greedy 配分** — スコア降順に token 数を積み上げ、budget を超えたら打ち切り
4. **バンドル出力** — 選択ファイルを Markdown に展開し、直近 Git 差分とともに出力

## 対応言語

| 言語 | シンボル抽出 | import 依存解析 |
|---|---|---|
| Rust | ✅ 関数/構造体/impl/use | ✅ `crate::` パス解決 |
| Python | ✅ 関数/クラス/import | ✅ モジュールパス解決 |
| Go | ✅ 関数/型/import | ✅ パッケージパス解決 |
| TypeScript | ✅ 関数/クラス/interface/type/enum/arrow/import | ✅ ES import 相対パス解決(拡張子・index.ts 補完) |
| JavaScript | ✅ 関数/クラス/arrow/import/require | ✅ ES import + CommonJS `require()` 相対パス解決 |

TypeScript は `.ts` / `.tsx` / `.mts` / `.cts`、JavaScript は `.js` / `.jsx` / `.mjs` / `.cjs` を対象とする。
bare import(`react` など node_modules パッケージ)は依存グラフには含めない。

## 精度

公開 Rust リポジトリ 3 つの、マージ済みの修正 PR で測定。タスク = その PR が閉じた Issue の題名、正解 = PR が変更した `src/**/*.rs`、インデックスは PR の**マージ前**のコミットで作成(修正の漏れ込みなし)。ファイルの内容が 50% 以上バンドルに入っている場合だけ「見つかった」と数える。比較対象は素朴な単語一致(タスクの語を含む数でファイルを並べ、パス一致は ×3 で加点、同じトークン予算を埋める)。タスク平均の再現率(recall):

| リポ(タスク数) | 予算 | context-smith | 素朴な単語一致 |
|---|---|---|---|
| axum (54) | 4k / 8k / 16k | 0.23 / 0.33 / 0.45 | 0.17 / 0.49 / 0.56 |
| clap (60) | 4k / 8k / 16k | 0.08 / 0.15 / 0.28 | 0.16 / 0.27 / 0.38 |
| bat (56) | 4k / 8k / 16k | 0.08 / 0.08 / 0.10 | 0.18 / 0.24 / 0.42 |

この条件では、axum の最小予算を除き、context-smith は素朴な単語一致に**勝てていません**。bat では正しいファイルの 88% を検索で拾えているが、順位が低く予算に入らない。限界: Rust リポジトリ 3 つのみ、BM25 のみ(埋め込みなし)、比較対象は未チューニング、他の言語は未測定。この数字の再現手順(スクリプト・固定した候補・指標の定義): [bench/](./bench/README.md)。未解決の点: [#135](https://github.com/flipslidersand-labs/context-smith/issues/135)。

## ステータス

| Phase | 内容 | 状態 |
|---|---|---|
| 1 | Git 走査・ファイル一覧取得 | ✅ |
| 2 | Tree-sitter シンボル抽出 (Rust/Python/Go) | ✅ |
| 3 | 依存グラフ構築 (petgraph BFS) | ✅ |
| 4 | BM25 全文検索 (SQLite FTS5) | ✅ |
| 5 | 予算配分 + バンドル出力 | ✅ |
| 6 | ローカル埋め込みモデル + RRF 統合 | 🔲 計画中 |

## ライセンス

MIT
