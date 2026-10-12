mod common;

use std::env;
use std::path::Path;

use common::TestRepo;

/// M0 acceptance: with the binary on PATH, git runs it as the `carve` subcommand.
#[test]
fn git_finds_carve_on_path() {
    let bin_dir = Path::new(env!("CARGO_BIN_EXE_git-carve")).parent().unwrap();
    let inherited = env::var_os("PATH").unwrap_or_default();
    let path =
        env::join_paths(std::iter::once(bin_dir.to_path_buf()).chain(env::split_paths(&inherited)))
            .unwrap();
    let repo = TestRepo::new();

    let version = repo
        .git()
        .clone()
        .env("PATH", path)
        .run_str(["carve", "--version"])
        .unwrap();

    assert_eq!(version, format!("git-carve {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_exits_zero() {
    let out = TestRepo::new().carve().arg("--help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(help.contains("Usage: git carve"), "{help}");
}

/// Usage errors exit 129 like git's own commands, never 2 (a refused repository state).
#[test]
fn usage_errors_exit_129() {
    for args in [
        &["--check", "true", "--no-check"][..],
        &["--judge", "--no-judge"],
        &["--undo", "--plan-only"],
        &["--clean", "--undo"],
        &["--bogus"],
    ] {
        let out = TestRepo::new().carve().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(129), "args: {args:?}");
    }
}
