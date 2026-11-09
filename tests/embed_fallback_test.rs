//! #262: when the embedding service is unreachable, `query` must warn and continue with
//! the BM25 seeds (exit 0) instead of failing.
#![cfg(feature = "remote-embed")]

use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(st.success(), "git {args:?} failed");
}

fn bin(args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(args)
        .env_remove("EMBEDDING_SVC_URL")
        .envs(envs.iter().copied())
        .output()
        .expect("run contextsmith")
}

#[test]
fn query_falls_back_to_bm25_when_embedding_service_unreachable() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    git(d, &["config", "user.email", "t@example.com"]);
    git(d, &["config", "user.name", "t"]);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(
        d.join("src/lib.rs"),
        "// zebrafish quantizer\npub fn zebrafish_quantizer() -> u32 { 1 }\n",
    )
    .unwrap();
    git(d, &["add", "src/lib.rs"]);
    git(d, &["commit", "-q", "-m", "init"]);
    let repo = d.to_str().unwrap();
    let out = bin(&["index", "--repo", repo], &[]);
    assert!(out.status.success());

    // Give the index a stored embedding so the embedder is actually called.
    let conn = rusqlite::Connection::open(d.join(".contextsmith/index.db")).unwrap();
    conn.execute(
        "INSERT INTO embeddings (file_id, vector) SELECT id, x'0000803f' FROM files",
        [],
    )
    .unwrap();
    drop(conn);

    // A port that was free a moment ago: connections are refused.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}");
    let out = bin(
        &["query", "--repo", repo, "--task", "zebrafish quantizer"],
        &[("EMBEDDING_SVC_URL", &url)],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit {:?}: {stderr}",
        out.status.code()
    );
    assert!(
        stderr.contains("semantic: embedding failed") && stderr.contains("falling back to BM25"),
        "missing fallback warning: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("src/lib.rs"),
        "BM25 result missing: {stdout}"
    );
}
