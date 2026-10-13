use std::process::Command;

fn version_output(flag: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .arg(flag)
        .output()
        .unwrap();
    assert!(out.status.success(), "{flag} should exit 0");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn long_flag_prints_name_and_version() {
    let expected = format!("contextsmith {}", env!("CARGO_PKG_VERSION"));
    assert_eq!(version_output("--version").trim(), expected);
}

#[test]
fn short_flag_matches_long_flag() {
    assert_eq!(version_output("-V"), version_output("--version"));
}
