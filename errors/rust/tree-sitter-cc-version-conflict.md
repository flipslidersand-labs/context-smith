---
title: "tree-sitter グラマー間の cc バージョン競合"
tags: [rust, tree-sitter, cargo, dependency]
severity: medium
date: "2026-08-22"
---

## 症状

`cargo add tree-sitter-ruby` が失敗する。

```
error: failed to select a version for `cc`.
  tree-sitter-javascript v0.21.0 requires cc = "~1.0.90"
  tree-sitter-ruby v0.23.1 requires cc = "^1.1"
```

## 原因

`tree-sitter-javascript 0.21.x` は `cc ~1.0.90`（旧 API）を要求するが、
`tree-sitter-ruby 0.23.x` は `cc ^1.1`（新 API）を要求するため競合する。

## 解決策

全グラマーを同一メジャー系列に統一する。

```toml
# Cargo.toml
tree-sitter  = "0.24"
tree-sitter-rust       = "0.23"
tree-sitter-python     = "0.23"
tree-sitter-go         = "0.23"
tree-sitter-typescript = "0.23"
tree-sitter-javascript = "0.23"
tree-sitter-ruby       = "0.23"
```

また 0.22 → 0.24 で `language()` 関数が廃止され、`LANGUAGE` 定数に変更。

```rust
// Before (0.22)
parser.set_language(&tree_sitter_rust::language())
// After (0.24)
parser.set_language(&tree_sitter_rust::LANGUAGE.into())
// TypeScript TSX
parser.set_language(&tree_sitter_typescript::LANGUAGE_TSX.into())
```

## 予防

新グラマーを追加する前に `cargo info <crate>` で `cc` 依存バージョンを確認する。
既存グラマーと同一系列のバージョンを選ぶ。
