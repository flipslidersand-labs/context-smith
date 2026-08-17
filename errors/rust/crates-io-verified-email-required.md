---
title: "cargo publish が 400 verified email required で失敗"
tags: [cargo, crates-io, publish, rust]
severity: medium
date: "2026-08-17"
---

## 症状

初回の `cargo publish` で、パッケージングとアップロードは進むが最後に 400 で失敗する:

```
error: failed to publish ... to registry at https://crates.io
Caused by:
  the remote server responded with an error (status 400 Bad Request):
  A verified email address is required to publish crates to crates.io.
  Visit https://crates.io/settings/profile to set and verify your email address.
```

## 原因

crates.io は publish 時に**アカウントのメールアドレス認証**を要求する。
API トークンやパッケージ内容の問題ではなく、アカウント側の設定不足。
`cargo login` が通っていても、メール未認証だと publish は 400 で弾かれる。

## 解決策

1. https://crates.io/settings/profile でメールアドレスを設定
2. 届いた確認メールの**リンクを実際にクリック**して verify
3. `cargo publish` を再実行（トークン再生成は不要）

## 予防

- 新規クレートを publish する前に、crates.io プロフィールのメール認証を先に済ませておく。
- API トークンはチャットやリポジトリに貼らず、`cargo login` の対話入力にのみ渡す。
