//! D45: the generic adapter and ecosystem checks. Outside TypeScript, a feature still becomes
//! one commit across files, a drive-by fix stays apart, hunks show their function's name, and
//! the default check fits the ecosystem (cargo, go, python), or carve says how to set one.

mod common;

use std::path::Path;
use std::process::Command;

use common::{TestRepo, atoms_in, group_of, stderr};

fn on_path(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

fn plan_json(repo: &TestRepo, extra: &[&str]) -> serde_json::Value {
    let out = repo
        .carve()
        .args(["--plan-only", "--json"])
        .args(extra)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// A Python project: a new `load_json` in core.py, used in cli.py and tested; plus an
/// unrelated `.strip()` fix inside `parse`.
fn python_project() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("pyproject.toml", "[project]\nname = \"app\"\n");
    repo.write("app/core.py", "def load(path):\n    with open(path) as f:\n        return f.read()\n\n\ndef parse(text):\n    return text.split(\",\")\n\n\ndef total(items):\n    return sum(int(x) for x in items)\n");
    repo.write("app/cli.py", "import sys\nfrom app.core import load, parse, total\n\n\ndef main():\n    print(total(parse(load(sys.argv[1]))))\n");
    repo.commit_all("base");
    repo.write("app/core.py", "import json\n\n\ndef load(path):\n    with open(path) as f:\n        return f.read()\n\n\ndef parse(text):\n    return [x.strip() for x in text.split(\",\")]\n\n\ndef total(items):\n    return sum(int(x) for x in items)\n\n\ndef load_json(path):\n    with open(path) as f:\n        return json.load(f)\n");
    repo.write("app/cli.py", "import sys\nfrom app.core import load, load_json, parse, total\n\n\ndef main():\n    path = sys.argv[1]\n    items = load_json(path) if path.endswith(\".json\") else parse(load(path))\n    print(total(items))\n");
    repo
}

#[test]
fn a_python_feature_is_one_commit_and_the_drive_by_fix_stays_apart() {
    let repo = python_project();
    let (changes, _, plan) = repo.plan();
    let core = atoms_in(&changes, "app/core.py");
    let cli = atoms_in(&changes, "app/cli.py");
    assert_eq!(core.len(), 3, "import json, the parse fix, load_json");
    let feature = group_of(&plan, core[2]).expect("load_json is committed");
    for id in [core[0], cli[0], cli[1]] {
        assert_eq!(
            group_of(&plan, id),
            Some(feature),
            "{id} belongs to the load_json feature"
        );
    }
    let fix = group_of(&plan, core[1]).expect("the fix is committed");
    assert_ne!(
        fix, feature,
        "the drive-by fix in parse stays its own commit"
    );
    assert_eq!(plan.groups[fix], vec![core[1]]);

    // Hunks are named by their function, as in TypeScript.
    let doc = plan_json(&repo, &["--no-check"]);
    let symbol = |id: &str| {
        doc["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .map(|a| a["symbol"].clone())
            .unwrap()
    };
    assert_eq!(symbol(&core[2].to_string()), "load_json");
    assert_eq!(symbol(&core[1].to_string()), "parse");
}

#[test]
fn a_python_project_gets_a_syntax_check_and_the_probe_proves_it() {
    if !on_path("python3") {
        return;
    }
    let repo = python_project();
    let doc = plan_json(&repo, &[]);
    assert_eq!(
        doc["verification"]["check"],
        "python3 -m compileall (syntax only)"
    );
    assert!(
        doc["verification"]["probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["caught"] == true),
        "a deliberately broken .py fails the check: {}",
        doc["verification"]["probes"]
    );
    assert!(
        !repo.path().join("app/__pycache__").exists(),
        "the check ran in the scratch worktree"
    );
}

#[test]
fn go_definitions_link_across_files() {
    let repo = TestRepo::new();
    repo.write("go.mod", "module example.com/app\n\ngo 1.22\n");
    repo.write(
        "main.go",
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(fetch())\n}\n",
    );
    repo.write(
        "fetch.go",
        "package main\n\nfunc fetch() string {\n\treturn \"ok\"\n}\n",
    );
    repo.write(
        "parse.go",
        "package main\n\nfunc parse(s string) int {\n\treturn len(s) - 1\n}\n",
    );
    repo.commit_all("base");
    repo.write(
        "main.go",
        "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(withRetry(fetch))\n}\n",
    );
    repo.write("retry.go", "package main\n\nfunc withRetry(f func() string) string {\n\tfor i := 0; i < 3; i++ {\n\t\tif r := f(); r != \"\" {\n\t\t\treturn r\n\t\t}\n\t}\n\treturn \"\"\n}\n");
    repo.write(
        "parse.go",
        "package main\n\nfunc parse(s string) int {\n\treturn len(s)\n}\n",
    );
    let (changes, _, plan) = repo.plan();
    let retry = atoms_in(&changes, "retry.go");
    let main = atoms_in(&changes, "main.go");
    let parse = atoms_in(&changes, "parse.go");
    assert_eq!(
        group_of(&plan, retry[0]),
        group_of(&plan, main[0]),
        "new func and its use"
    );
    assert_ne!(
        group_of(&plan, parse[0]),
        group_of(&plan, main[0]),
        "the drive-by fix"
    );
    assert_eq!(plan.groups.len(), 2, "{:?}", plan.groups);
}

#[test]
fn a_rust_crate_is_checked_with_cargo_check() {
    if !on_path("cargo") {
        return;
    }
    let repo = TestRepo::new();
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    );
    repo.write("src/lib.rs", "pub mod util;\n\npub fn run() -> u32 {\n    util::base() + 1\n}\n\npub fn width(s: &str) -> usize {\n    s.len() + 1\n}\n");
    repo.write("src/util.rs", "pub fn base() -> u32 {\n    41\n}\n");
    repo.write(".gitignore", "/target\n");
    repo.commit_all("base");
    repo.write(
        "src/util.rs",
        "pub fn base() -> u32 {\n    41\n}\n\npub fn retry_count() -> u32 {\n    3\n}\n",
    );
    repo.write("src/lib.rs", "pub mod util;\n\npub fn run() -> u32 {\n    util::base() + util::retry_count()\n}\n\npub fn width(s: &str) -> usize {\n    s.len()\n}\n");
    let doc = plan_json(&repo, &[]);
    assert_eq!(doc["verification"]["check"], "cargo check --all-targets");
    assert!(
        doc["verification"]["probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["caught"] == true)
    );
    assert_eq!(
        doc["groups"].as_array().unwrap().len(),
        2,
        "{}",
        doc["groups"]
    );
    assert!(
        !Path::new(&repo.path().join("target")).exists(),
        "builds go to .git/carve"
    );
}

#[test]
fn a_file_the_check_does_not_read_is_unverified_not_probed() {
    if !on_path("python3") {
        return;
    }
    // A Python project with a Rust helper script: compileall reads .py, not .rs.
    let repo = python_project();
    repo.write("tools/gen.rs", "fn main() {}\n");
    repo.commit_all("add a tool");
    repo.write("app/cli.py", "import sys\nfrom app.core import load, parse, total\n\n\ndef main():\n    print(total(parse(load(sys.argv[1]))) + 0)\n");
    repo.write("tools/gen.rs", "fn main() {\n    println!(\"gen\");\n}\n");
    let doc = plan_json(&repo, &[]);
    assert_eq!(
        doc["verification"]["unverified"],
        serde_json::json!(["tools/gen.rs"])
    );
    assert!(
        doc["verification"]["probes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["path"] != "tools/gen.rs"),
        "the probe only breaks files the check reads"
    );
}

#[test]
fn an_unknown_ecosystem_is_told_how_to_set_a_check() {
    let repo = TestRepo::new();
    repo.write("Makefile", "test:\n\ttrue\n");
    repo.write("src/a.c", "int a(void) {\n    return 1;\n}\n");
    repo.commit_all("base");
    repo.write("src/a.c", "int a(void) {\n    return 2;\n}\n");
    let out = repo.carve().args(["--plan-only"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("no default check for this repository"),
        "{err}"
    );
    assert!(err.contains("--check \"make test\""), "{err}");
    assert!(
        !err.contains("npx tsc"),
        "no TypeScript advice for a C project: {err}"
    );
}

/// D54: a JavaScript project. `fmtSize` is added to fmt.js and used in main.js; other.js gets
/// an unrelated fix. The new helper and its use land in one commit.
#[test]
fn javascript_definitions_link_across_files() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        "{ \"name\": \"app\", \"type\": \"module\" }\n",
    );
    repo.write(
        "src/fmt.js",
        "export function fmt(n) {\n  return `${n}`;\n}\n",
    );
    repo.write(
        "src/main.js",
        "import { fmt } from './fmt.js';\n\nexport function main(n) {\n  return fmt(n);\n}\n",
    );
    repo.write("src/other.js", "export const answer = () => 41;\n");
    repo.commit_all("base");
    repo.write("src/fmt.js", "export function fmt(n) {\n  return `${n}`;\n}\n\nexport function fmtSize(n) {\n  return `${n} B`;\n}\n");
    repo.write("src/main.js", "import { fmt, fmtSize } from './fmt.js';\n\nexport function main(n) {\n  return fmt(n) + fmtSize(n);\n}\n");
    repo.write("src/other.js", "export const answer = () => 42;\n");
    let doc = plan_json(&repo, &["--check", "true"]);
    let group_of = |path: &str| -> usize {
        let ids: Vec<&serde_json::Value> = doc["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["path"] == path)
            .map(|a| &a["id"])
            .collect();
        let groups = doc["groups"].as_array().unwrap();
        groups
            .iter()
            .position(|g| g.as_array().unwrap().iter().any(|id| ids.contains(&id)))
            .unwrap_or_else(|| panic!("{path} in no group: {doc}"))
    };
    assert_eq!(group_of("src/fmt.js"), group_of("src/main.js"), "{doc}");
    assert_ne!(group_of("src/other.js"), group_of("src/main.js"), "{doc}");
}

/// D54 review, must-change: tsc does not read `next.config.js` (no allowJs). A change to it
/// alone is reported as unverified; it is not "a vacuous default check".
#[test]
fn a_javascript_file_tsc_skips_is_unverified_not_refused() {
    let repo = TestRepo::demo();
    if !repo.link_typescript() {
        return;
    }
    repo.write(
        "next.config.js",
        "export default { reactStrictMode: true };\n",
    );
    repo.commit_all("everything so far");
    repo.write(
        "next.config.js",
        "export default { reactStrictMode: false };\n",
    );
    let out = repo.carve().args(["--plan-only"]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
    assert!(text.contains("next.config.js"), "{text}");
    assert!(text.contains("unverified"), "{text}");
}

/// D54: with allowJs and checkJs, tsc reads JavaScript, so the probe proves a `.js` change.
#[test]
fn a_javascript_file_under_allow_js_is_proved_by_the_probe() {
    let repo = TestRepo::new();
    if !repo.link_typescript() {
        return;
    }
    repo.write(".gitignore", "node_modules/\n");
    repo.write("tsconfig.json", "{ \"compilerOptions\": { \"allowJs\": true, \"checkJs\": true, \"noEmit\": true, \"target\": \"ES2022\", \"module\": \"ESNext\", \"moduleResolution\": \"Bundler\" }, \"include\": [\"src\"] }\n");
    repo.write(
        "src/util.js",
        "export function twice(n) {\n  return n * 2;\n}\n",
    );
    repo.commit_all("base");
    repo.write("src/util.js", "export function twice(n) {\n  return n * 2;\n}\n\nexport function thrice(n) {\n  return n * 3;\n}\n");
    let doc = plan_json(&repo, &[]);
    let verification = &doc["verification"];
    let probes = verification["probes"].as_array().unwrap();
    assert!(
        probes
            .iter()
            .any(|p| p["path"] == "src/util.js" && p["caught"] == true),
        "{verification}"
    );
    assert!(
        verification["unverified"].as_array().unwrap().is_empty(),
        "{verification}"
    );
}

/// D54 result review, finding 1: a JavaScript probe's verdict is remembered apart
/// from a TypeScript one in the same folder, in both orders.
#[test]
fn javascript_and_typescript_probe_verdicts_are_remembered_apart() {
    let repo = TestRepo::demo();
    if !repo.link_typescript() {
        return;
    }
    repo.write("src/legacy.js", "export const legacy = 1;\n");
    repo.commit_all("everything so far");
    let plan = |repo: &TestRepo| -> serde_json::Value {
        let out = repo
            .carve()
            .args(["--plan-only", "--json"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let format = std::fs::read_to_string(repo.path().join("src/format.ts")).unwrap();
    // JavaScript first: tsc (no allowJs) misses it. Then TypeScript in the same folder.
    repo.write("src/legacy.js", "export const legacy = 2;\n");
    let js = plan(&repo);
    assert!(
        js["verification"]["unverified"]
            .to_string()
            .contains("src/legacy.js"),
        "{js}"
    );
    repo.run(&["checkout", "--", "src/legacy.js"]);
    repo.write(
        "src/format.ts",
        format!("{format}export const unit = 'B';\n"),
    );
    let ts = plan(&repo);
    assert!(
        ts["verification"]["unverified"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the TypeScript change is proved, not refused or unverified: {ts}"
    );
    // TypeScript first, from a clean cache: the JavaScript file is still unverified.
    repo.run(&["checkout", "--", "src/format.ts"]);
    repo.carve().arg("--clean").output().unwrap();
    repo.write(
        "src/format.ts",
        format!("{format}export const unit = 'B';\n"),
    );
    plan(&repo);
    repo.run(&["checkout", "--", "src/format.ts"]);
    repo.write("src/legacy.js", "export const legacy = 3;\n");
    let js = plan(&repo);
    assert!(
        js["verification"]["unverified"]
            .to_string()
            .contains("src/legacy.js"),
        "{js}"
    );
}

/// Finding 2: a JavaScript file with more atoms must not take a folder's probe from the
/// TypeScript file, or a check that reads nothing there goes undetected.
#[test]
fn a_javascript_file_never_takes_the_typescript_probe() {
    let repo = TestRepo::new();
    if !repo.link_typescript() {
        return;
    }
    repo.write(".gitignore", "node_modules/\n");
    // The check reads only other/, so it is vacuous for src/.
    repo.write(
        "tsconfig.json",
        "{ \"compilerOptions\": { \"noEmit\": true }, \"include\": [\"other\"] }\n",
    );
    repo.write("other/x.ts", "export const x = 1;\n");
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.write(
        "src/legacy.js",
        "export const p = 1;\n\n\n\nexport const q = 1;\n\n\n\nexport const r = 1;\n",
    );
    repo.commit_all("base");
    repo.write("src/a.ts", "export const a = 2;\n");
    repo.write(
        "src/legacy.js",
        "export const p = 2;\n\n\n\nexport const q = 2;\n\n\n\nexport const r = 2;\n",
    );
    let out = repo.carve().args(["--plan-only"]).output().unwrap();
    assert!(
        !out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr(&out).contains("deliberately broken src/a.ts"),
        "{}",
        stderr(&out)
    );
}

/// D61 (F8): a barrel line (`export * from './b'`) names no symbol, so it used to become a
/// commit of its own; it now goes with the new module it re-exports. `import './polyfill'`
/// likewise. An unrelated change stays apart.
#[test]
fn a_barrel_line_goes_with_the_new_module_it_reexports() {
    let repo = TestRepo::new();
    repo.write("src/index.ts", "export * from './a';\n");
    repo.write("src/main.ts", "export const main = 1;\n");
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.commit_all("base");
    repo.write(
        "src/index.ts",
        "export * from './a';\nexport * from './b';\n",
    );
    repo.write("src/b.ts", "export const b = 2;\n");
    repo.write(
        "src/main.ts",
        "import './polyfill';\nexport const main = 1;\n",
    );
    repo.write("src/polyfill.ts", "export {};\n");
    repo.write("src/a.ts", "export const a = 3;\n");
    let doc = plan_json(&repo, &["--check", "true"]);
    let group_of = |path: &str| -> usize {
        let ids: Vec<&serde_json::Value> = doc["atoms"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["path"] == path)
            .map(|a| &a["id"])
            .collect();
        doc["groups"]
            .as_array()
            .unwrap()
            .iter()
            .position(|g| g.as_array().unwrap().iter().any(|id| ids.contains(&id)))
            .unwrap_or_else(|| panic!("{path} in no group: {doc}"))
    };
    assert_eq!(group_of("src/index.ts"), group_of("src/b.ts"), "{doc}");
    assert_eq!(
        group_of("src/main.ts"),
        group_of("src/polyfill.ts"),
        "{doc}"
    );
    assert_ne!(group_of("src/a.ts"), group_of("src/b.ts"), "{doc}");
}
