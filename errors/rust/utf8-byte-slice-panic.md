---
title: "String の byte-index スライスが non-char-boundary でパニック"
tags: [rust, utf8, string]
severity: high
date: "2026-08-16"
---

## 症状

`&content[..N]` で N がマルチバイト文字の途中に当たると
`byte index N is not a char boundary` でパニック。

## 原因

Rust の `&str` / `String` の添字 `[a..b]` は byte offset。
UTF-8 の 2〜4 バイト文字の途中を指すと即パニック。

## 解決策

```rust
// 安全な切り捨て: chars().take() を使う
let truncated: String = content.chars().take(MAX_CHARS).collect();

// または nightly: floor_char_boundary
let limit = content.floor_char_boundary(MAX_BYTES);
let truncated = &content[..limit];
```

512KB バイト制限を文字数制限 `512*1024/4` に変換するのが最も安全。

## 予防

`String` を固定バイト数で切る場合は必ず char boundary チェックを入れる。
`content.len()` は bytes。`content.chars().count()` は文字数。混同注意。
