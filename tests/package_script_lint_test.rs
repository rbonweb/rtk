#![cfg(unix)]
//! A package script named `lint` runs as the script, whatever it contains.
//!
//! The hook used to rewrite `pnpm lint` / `npm run lint` to `rtk lint`, which ran
//! `eslint -f json .` instead of the project's script: a biome project got a JSON parse
//! error, and a script like `oxlint ... && eslint . --max-warnings 0` lost half its checks
//! and could pass when it should fail. The script runner must keep the script's own argv
//! and exit code.

mod common;

use std::path::Path;
use std::process::Output;

fn npm_available() -> bool {
    let mut cmd = common::native_command("npm");
    cmd.arg("--version");
    cmd.output().is_ok_and(|out| out.status.success())
}

fn project_with_lint_script(script: &str) -> tempfile::TempDir {
    let dir = common::tempdir();
    let manifest = format!(
        "{{\"name\":\"lint-script-fixture\",\"version\":\"1.0.0\",\"scripts\":{{\"lint\":{}}}}}",
        serde_json::to_string(script).expect("script serializes")
    );
    std::fs::write(dir.path().join("package.json"), manifest).expect("write package.json");
    dir
}

fn rtk_in(dir: &Path, args: &[&str]) -> Output {
    common::rtk_command()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn rtk")
}

fn rewritten(command: &str) -> String {
    let out = common::rtk_command()
        .args(["rewrite", command])
        .output()
        .expect("spawn rtk rewrite");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn npm_run_lint_runs_the_script_and_keeps_its_exit_code() {
    if !npm_available() {
        return;
    }
    let project = project_with_lint_script("echo LINT-SCRIPT-RAN && exit 3");

    let out = rtk_in(project.path(), &["npm", "run", "lint"]);

    assert_eq!(out.status.code(), Some(3), "the script's exit code is kept");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("LINT-SCRIPT-RAN"),
        "the script's own output reaches the caller, got: {stdout}"
    );
}

#[test]
fn npm_run_lint_never_adds_json_flags_and_passes_arguments_through() {
    if !npm_available() {
        return;
    }
    // The script prints its argv one per line, so anything rtk adds would show up here.
    let project = project_with_lint_script("printf 'ARG:%s\\n'");

    let out = rtk_in(
        project.path(),
        &["npm", "run", "lint", "--", "--fix", "src"],
    );

    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ARG:--fix"), "got: {stdout}");
    assert!(stdout.contains("ARG:src"), "got: {stdout}");
    for injected in ["ARG:-f", "ARG:json", "ARG:."] {
        assert!(
            !stdout.lines().any(|line| line == injected),
            "rtk must not add `{injected}` to a package script, got: {stdout}"
        );
    }
}

#[test]
fn npm_rum_alias_runs_the_named_script() {
    if !npm_available() {
        return;
    }
    let project = project_with_lint_script("echo LINT-SCRIPT-RAN && exit 3");

    let out = rtk_in(project.path(), &["npm", "rum", "lint"]);

    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stdout).contains("LINT-SCRIPT-RAN"));
}

#[test]
fn rewrite_routes_lint_scripts_to_the_script_runner() {
    for (raw, expected) in [
        ("pnpm lint", "rtk pnpm lint"),
        ("pnpm run lint", "rtk pnpm run lint"),
        ("pnpm run-script lint", "rtk pnpm run-script lint"),
        ("npm run lint", "rtk npm run lint"),
        ("npm run-script lint", "rtk npm run-script lint"),
        ("npm rum lint", "rtk npm rum lint"),
        ("bun run lint", "rtk bun run lint"),
        ("pnpm lint --fix", "rtk pnpm lint --fix"),
        ("npm run lint -- --fix", "rtk npm run lint -- --fix"),
    ] {
        assert_eq!(rewritten(raw), expected, "rewrite of `{raw}`");
    }
}

#[test]
fn rewrite_keeps_explicit_tools_on_the_lint_filter() {
    for (raw, expected) in [
        ("biome check", "rtk lint biome check"),
        ("pnpm biome check", "rtk lint biome check"),
        ("pnpm exec biome check", "rtk lint biome check"),
        ("npx biome check src", "rtk lint biome check src"),
        ("eslint .", "rtk lint eslint ."),
        ("npx eslint .", "rtk lint eslint ."),
        ("pnpm exec eslint src", "rtk lint eslint src"),
        ("bunx eslint", "rtk bunx eslint"),
        ("bunx biome check", "rtk bunx biome check"),
    ] {
        assert_eq!(rewritten(raw), expected, "rewrite of `{raw}`");
    }
}

#[test]
fn rewrite_leaves_unknown_lint_commands_alone() {
    // Not a package script of a manager rtk routes, and not a linter: nothing to substitute.
    for raw in ["lint", "yarn lint"] {
        assert_eq!(rewritten(raw), "", "`{raw}` must not be rewritten");
    }
}
