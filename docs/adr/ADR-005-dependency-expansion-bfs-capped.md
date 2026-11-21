# ADR-005: 依存展開の既定を bfs-capped にする

- **日付**: 2026-11-21
- **状態**: Accepted
- **関連**: [#293](https://github.com/flipslidersand-labs/context-smith/issues/293)、PR [#508](https://github.com/flipslidersand-labs/context-smith/pull/508)(切替の追加)・[#513](https://github.com/flipslidersand-labs/context-smith/pull/513)(既定の変更)

## 背景

BM25 で選んだ seed ファイルから、依存グラフをたどってスコアを広げる(`src/expand.rs`)。従来の `bfs`(深さ 2、1 hop ごとに x0.5)は、多くのファイルとつながるハブに点が集まりやすかった。展開方法を `CONTEXTSMITH_EXPAND` で切り替えられるようにし(#508)、3 方式を同じベンチで比べた。

採用の基準は計測の前に決めた: `eval` グループ(ripgrep・fd・hono・zod。パラメータ調整に使っていない)の平均 recall が +0.02 以上、どのセルも 0.04 を超えて下がらない、時間が悪化しない。

## 決定

既定を `bfs-capped` にする。各ファイルが広げる先は次数の低い順に最大 10 件、隣のスコアは `parent * 0.5 / sqrt(degree)`。`CONTEXTSMITH_EXPAND=bfs` で従来の挙動に戻せる。

| グループ | セル数 | `bfs` 平均 | `bfs-capped` 平均 | 差 | 最悪セル |
|---|---|---|---|---|---|
| `eval` | 12 | 0.457 | 0.477 | +0.020 | fd 8k -0.03 |
| `tuning` | 15 | 0.393 | 0.417 | +0.024 | bat 4k -0.02 |

出典: [bench/README.md](../../bench/README.md#dependency-expansion-bfs-vs-capped-bfs-vs-pagerank-293)(2026-11-21、master `53e4db1` の release ビルド、strict・タイトルのみ、各リポ 1 回)。判断の根拠は `eval` の +0.020 だけで、`tuning` は調整に使ったため「下がるセルが無いか」の確認にだけ使った。

## 却下した代替案

- **`ppr`(personalized PageRank、alpha 0.85、`bfs` の候補集合内で順位だけ変える)**: `eval` 平均 -0.010、最悪セル fd 8k -0.04 で基準を満たさない(同じ出典)。
- **`bfs` のまま**: 上の表のとおり `bfs-capped` が両グループで上回り、下げ幅も基準内のため。

## トレードオフ

- 改善幅 +0.020 は基準ちょうどで、1 タスクでセルが 0.01〜0.04 動くため誤差に近い。ベンチは決定的で、2 回目の実行は全セルで一致した(残る不確実性はタスク数の少なさ)。
- 改善は TypeScript の 2 リポ(hono・zod)に偏り、Rust の 2 リポ(ripgrep・fd)は横ばいか微減。
- 精度目標 T1 は変わらない(poetry の 8k・16k は両方式とも基準線を下回る)。
