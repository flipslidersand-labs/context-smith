# context-smith

Release type: `library`

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
| `--force` | off | 内容が変わっていないファイルも含めて全件再インデックス |

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
| `--index <PATH>` | `{repo}/.contextsmith/index.db` | インデックスパス上書き(`--out` で作った場合に指定) |
| `--explain` | off | task.md に BM25 スコアを表示 |
| `--diff-commits <N>` | `3` | 差分に含める直近コミット数 |
| `--no-embed` | off | インデックスに埋め込みがあっても意味検索シードを使わず BM25 のみにする(`remote-embed` feature 付きビルドでのみ有効) |

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
| `--no-embed` | off | インデックスに埋め込みがあっても意味検索シードを使わず BM25 のみにする(`remote-embed` feature 付きビルドでのみ有効) |

JSON 出力は `{ task, budget, used_tokens, files: [{ path, score, tokens, truncated }] }` 形式。`truncated` は残り予算に収めるためファイルを切り詰めた場合に `true`。

## アーキテクチャ

```
contextsmith index          contextsmith build
      │                           │
      ▼                           ▼
 [GitRepo.scan()]         [search_bm25()]       ← FTS5 UNION ALL
 [SymbolExtractor]        [bfs_expand()]        ← SQL ベース BFS
 [build_deps()]           [allocate()]          ← greedy budget
 [populate_fts()]         [write_bundle()]      ← Markdown + JSON
      │
      ▼
 .contextsmith/index.db (SQLite)
 ├── files        — パス・言語・作業ツリーの内容の blob SHA(未コミットの編集も索引に反映。対象は HEAD で追跡中のファイル)
 ├── symbols      — 関数/構造体/クラス名・行番号・スニペット
 ├── deps         — ファイル間の import 依存グラフ
 ├── meta         — repo_root・indexed_at
 ├── fts_path     — FTS5 (トークン化したファイルパス、BM25 weight ×3)
 ├── fts_symbols  — FTS5 (pre-tokenized、シンボル 1 件 1 行、BM25 weight ×0.5)
 └── fts_body     — FTS5 (ファイル本文、BM25 weight ×1)
```

### スコアリングパイプライン

1. **BM25 検索** — タスク文字列で `fts_path`（重み×3）・`fts_symbols`（重み×0.5、ファイルごとに最良の一致）・`fts_body`（重み×1）を UNION ALL で検索、上位 20 件を seed として取得。タスクが 9 語以上(重複除く)のときは、先に出現ファイル数が少ない順の上位 8 語に絞る(一般語が固有の語を埋もれさせないため)
2. **依存グラフ BFS** — seed ファイルから bidirectional BFS（depth=2）でスコアを伝播（hop ごとに ×0.5 減衰）。既定では 1 ファイルが広げる先は最大 10 件で、次数の小さい順（`CONTEXTSMITH_EXPAND` = `bfs-capped`（既定）/ `bfs`（従来の素の BFS）/ `ppr`（実験的）で切替。詳細は上の「クエリ」節）
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
| Ruby | ✅ class/module/method/require (`.rb`) | ✅ `require_relative` は呼び出し元のディレクトリから、`require 'x'` は `lib/x.rb` か `x.rb` に解決(gem は解決しない) |

TypeScript は `.ts` / `.tsx` / `.mts` / `.cts`、JavaScript は `.js` / `.jsx` / `.mjs` / `.cjs` を対象とする。
bare import(`react` など node_modules パッケージ)は依存グラフには含めない。

## 精度

公開リポジトリ 9 つの、マージ済みの修正 PR で測定。Rust 5 つ(axum・clap・bat・ripgrep・fd)、Python 2 つ(scrapy・poetry)、TypeScript 2 つ(hono・zod)。タスク = その PR が閉じた Issue の題名、正解 = PR が変更したソースファイル(`src/**/*.rs`、`*.py`、`*.ts` / `*.tsx`。テストとドキュメントは除く)、インデックスは PR の**マージ前**のコミットで作成(修正の漏れ込みなし)。ファイルの内容が 50% 以上バンドルに入っている場合だけ「見つかった」と数える。比較対象は素朴な単語一致(タスクの語を含む数でファイルを並べ、パス一致は ×3 で加点、同じトークン予算を埋める)。タスク平均の再現率(recall)、context-smith / 素朴な単語一致(括弧内は差):

| リポ(タスク数) | 4k | 8k | 16k |
|---|---|---|---|
| axum (54) | 0.46 / 0.17 (+0.29) | 0.56 / 0.49 (+0.07) | 0.69 / 0.56 (+0.13) |
| clap (60) | 0.26 / 0.16 (+0.10) | 0.35 / 0.27 (+0.08) | 0.53 / 0.38 (+0.15) |
| bat (56) | 0.28 / 0.18 (+0.10) | 0.40 / 0.24 (+0.16) | 0.59 / 0.42 (+0.17) |
| ripgrep (26) | 0.15 / 0.08 (+0.07) | 0.22 / 0.19 (+0.03) | 0.49 / 0.27 (+0.22) |
| fd (74) | 0.40 / 0.25 (+0.15) | 0.57 / 0.37 (+0.20) | 0.75 / 0.69 (+0.06) |
| scrapy (59) | 0.29 / 0.19 (+0.10) | 0.42 / 0.29 (+0.13) | 0.48 / 0.47 (+0.01) |
| poetry (60) | 0.23 / 0.19 (+0.04) | 0.26 / 0.31 (**-0.05**) | 0.30 / 0.40 (**-0.10**) |
| hono (60) | 0.60 / 0.35 (+0.25) | 0.72 / 0.56 (+0.16) | 0.83 / 0.66 (+0.17) |
| zod (60) | 0.20 / 0.04 (+0.16) | 0.28 / 0.07 (+0.21) | 0.39 / 0.18 (+0.21) |

出典: [bench/README.md](./bench/README.md#targets-and-current-status-147)。2026-11-03 に master `038eadc`([#164](https://github.com/flipslidersand-labs/context-smith/pull/164) の後)で、release ビルドのバイナリ、リポごとに 1 回、strict 指標・題名のみのタスクで測定。再現: `REPO=<clone> TASKMODE=title python3 bench/bench.py`(準備とリポごとのオプションは [bench/](./bench/README.md))。1 タスクは 0.02〜0.04 に相当するので、0.03 以下の差は誤差の範囲。

**精度の目標は未達です([#135](https://github.com/flipslidersand-labs/context-smith/issues/135) は未解決)。** 下限(どのセルでも context-smith が単語一致 − 0.02 以上)は 27 セル中 25 で成立し、満たさないのは **poetry の 8k(-0.05)と 16k(-0.10)** の 2 つ。価値の目標(16k で単語一致を 0.05 以上上回る)は 9 リポ中 7 で成立し、外れるのは scrapy(+0.01)と poetry(-0.10)、どちらも Python。Rust の数字がそのまま通用するとは考えないこと。この測定では、poetry(0.26、0.30)と hono(0.72、0.83)で 8k から 16k に recall が下がる現象は起きていない。以前の値(master `aa4ef89`、[#164](https://github.com/flipslidersand-labs/context-smith/pull/164) より前)では起きており、[bench/](./bench/README.md) に残してある。正しいファイルが事前に分かっている場合に 16k に全文で入るものの上限(ceiling)は、axum 0.97、clap 0.87、bat 0.99、scrapy 0.97、poetry 0.99、hono 1.00、zod 0.68(bench による。ripgrep と fd は記載なし)。この数字は開発版のもの。予算配分・順位づけ・シンボル採点の修正([#142](https://github.com/flipslidersand-labs/context-smith/pull/142)・[#143](https://github.com/flipslidersand-labs/context-smith/pull/143)・[#151](https://github.com/flipslidersand-labs/context-smith/pull/151)・[#162](https://github.com/flipslidersand-labs/context-smith/pull/162))と [#164](https://github.com/flipslidersand-labs/context-smith/pull/164) は crates.io の 0.4.1 に入っている。[CHANGELOG.md](./CHANGELOG.md) のそれ以降の修正は未公開。限界: BM25 のみ(埋め込みなし)、比較対象は未チューニング、リポごとに 1 回の測定。シンボルの重み(0.5)と予算配分の規則は axum・clap・bat・scrapy・poetry で開発したため、この表はそれらのリポに対して楽観的。ripgrep・fd・hono・zod は、どのパラメータの選択にも使っていない。ripgrep は正解ファイルが大きく(中央値 約 9,400 トークン。他は約 3,500)、予算配分の規則が最も効く。この数字の再現手順(スクリプト・固定した候補・指標の定義): [bench/](./bench/README.md)。未解決の点: [#135](https://github.com/flipslidersand-labs/context-smith/issues/135)。

## ステータス

| Phase | 内容 | 状態 |
|---|---|---|
| 1 | Git 走査・ファイル一覧取得 | ✅ |
| 2 | Tree-sitter シンボル抽出 (Rust/Python/Go/TS/JS/Ruby) | ✅ |
| 3 | 依存グラフ構築 (SQL ベース BFS) | ✅ |
| 4 | BM25 全文検索 (SQLite FTS5) | ✅ |
| 5 | 予算配分 + バンドル出力 | ✅ |
| 6 | 埋め込みシード + RRF 統合(opt-in の `remote-embed` feature。`EMBEDDING_SVC_URL` の外部埋め込みサービスを使う) | ✅(ローカル埋め込みモデルは未実装) |

## ライセンス

MIT
