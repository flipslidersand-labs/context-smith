---
title: "FTS5 MATCH に自然言語をそのまま渡すと SQL エラー"
tags: [sqlite, fts5, rust]
severity: high
date: "2026-08-16"
---

## 症状

`WHERE col MATCH ?1` に `(auth)` や `"quoted"` など FTS5 演算子を含む文字列を渡すと、
`rusqlite` から `fts5: syntax error near ")"` のような Err が返る。

## 原因

FTS5 の MATCH は SQL の LIKE と異なりクエリ式として解釈される。
バインドパラメータ `?1` でも値はクエリ式として評価される。

## 解決策

MATCH に渡す前に特殊文字をスペースに置換する:

```rust
let sanitized = query.replace(['(', ')', '"', '*', '-', ':', '^'], " ");
```

または FTS5 の phrase query 形式 `"..."` で全体をダブルクォートで囲む
（ただし内部のダブルクォートをエスケープする必要あり）。

## 予防

ユーザー入力を直接 FTS5 MATCH に渡さない。
必ずサニタイズ関数を経由する。
