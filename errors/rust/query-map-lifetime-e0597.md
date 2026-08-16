---
title: "query_map の結果を collect するときのライフタイムエラー E0597"
tags: [rust, rusqlite, lifetime]
severity: medium
date: "2026-08-16"
---

## 症状

```
error[E0597]: `stmt` does not live long enough
```

`query_map(...)?` を `collect` してブロック末尾で返そうとすると `stmt` がブロック内で drop されるため、`?` が生成する一時値が `stmt` を参照したままになりエラー。

## 原因

rusqlite の `query_map()` は `stmt` を借用したイテレータを返す。
`?` 演算子が `ControlFlow` を生成する際にこの借用が一時値として残り、
`stmt` の drop より後まで生存しているとコンパイラが判断する。

```rust
// NG: stmt はブロック末尾で drop されるが、? の一時値が残る
let rows: Vec<_> = {
    let mut stmt = conn.prepare(...)?;
    stmt.query_map([], ...)?.collect::<rusqlite::Result<Vec<_>>>()?
};
```

## 解決策

`stmt` をブロック外に出すか、`drop(stmt)` を明示的に呼ぶ。

```rust
// OK: stmt を outer scope に出して collect 後に drop
let mut stmt = conn.prepare(...)?;
let rows: Vec<_> = stmt
    .query_map([], ...)?
    .collect::<rusqlite::Result<Vec<_>>>()?;
drop(stmt); // 以降で conn を再利用する場合は明示的 drop が必要
```

## 予防

query_map + collect パターンは stmt をローカルブロックに閉じ込めない。
conn を再利用するために stmt を早めに drop したい場合は `drop(stmt)` を明示する。
