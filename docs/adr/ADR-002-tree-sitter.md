# ADR-002: シンボル抽出に Tree-sitter を使う

- **日付**: 2026-06-20
- **状態**: Accepted

## 背景

コードからシンボルを抽出する方法として、正規表現・Tree-sitter・言語固有パーサーの選択肢がある。

## 決定

`tree-sitter` + 言語グラマー (`tree-sitter-rust`, `tree-sitter-python`) を使う。

## 理由

- 正規表現は `fn foo` を検出できるが、ネスト・マクロ・コメント内の偽陽性が多い
- Tree-sitter は構文木ベースで正確にシンボルを特定でき、S 式クエリで宣言的に書ける
- 将来的に Go・TypeScript を追加する際も `tree-sitter-<lang>` を追加するだけでよい
- `SafeCode Arena (R4)` と `ContextSmith` で同じクレートを共有でき、学習コストを共通化できる

## トレードオフ

- S 式クエリの文法に慣れるまで時間がかかる
- バイナリサイズが言語グラマー数に比例して増える
