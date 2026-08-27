---
title: "clippy::chunks_exact_to_as_chunks (Rust 1.98 新規 lint)"
tags: [rust, clippy, ci]
severity: low
date: "2026-08-22"
---

## 症状

CI (Rust 1.98) で clippy が失敗する。ローカル (Rust <1.98) では通過する。

```
error: use `as_chunks` instead of `chunks_exact`
  --> src/embed.rs:45:7
   |
45 |     b.chunks_exact(4)
   |       ^^^^^^^^^^^^^^^ help: consider using `as_chunks` instead: `as_chunks::<4>().0.iter()`
```

## 原因

Rust 1.98 で `clippy::chunks_exact_to_as_chunks` lint が追加された。
`-D warnings` が有効な CI でのみ error になる。

## 解決策

```rust
// Before
b.chunks_exact(4)
    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
    .collect()

// After
b.as_chunks::<4>()
    .0
    .iter()
    .map(|&c| f32::from_le_bytes(c))
    .collect()
```

## 予防

CI の Rust バージョンが上がるたびに新 lint が追加されることがある。
ローカルも定期的に `rustup update stable` して追従する。
