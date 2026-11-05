//! B5 (D48): a new use of a dependency travels with the manifest line that adds it.

mod common;

use common::TestRepo;

/// The hard binds B5 produced, as (path of a, path of b).
fn b5(repo: &TestRepo) -> Vec<(String, String)> {
    let (changes, _, plan) = repo.plan();
    let path = |id: git_carve::atom::AtomId| {
        String::from_utf8_lossy(&changes.files[changes.atom(id).file].path).into_owned()
    };
    plan.hard
        .iter()
        .filter(|(_, _, why)| why.contains("dependency"))
        .map(|&(a, b, _)| (path(a), path(b)))
        .collect()
}

#[test]
fn manifest_new_npm_dependency_binds_first_import() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        "{\n  \"name\": \"app\",\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n",
    );
    repo.write("src/client.ts", "export function get() {\n  return 1;\n}\n");
    repo.write("src/other.ts", "export const x = 1;\n");
    repo.commit_all("base");
    repo.write("package.json", "{\n  \"name\": \"app\",\n  \"dependencies\": {\n    \"axios\": \"^1.7.0\",\n    \"left-pad\": \"1.0.0\"\n  }\n}\n");
    repo.write(
        "src/client.ts",
        "import axios from \"axios\";\n\nexport function get() {\n  return axios.get(\"/x\");\n}\n",
    );
    repo.write("src/other.ts", "export const x = 2;\n");
    let binds = b5(&repo);
    assert!(
        binds.contains(&("src/client.ts".into(), "package.json".into())),
        "{binds:?}"
    );
    assert!(!binds.iter().any(|(a, _)| a == "src/other.ts"), "{binds:?}");
}

#[test]
fn manifest_new_file_using_a_new_dependency_binds() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n",
    );
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.commit_all("base");
    repo.write(
        "package.json",
        "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\",\n    \"zod\": \"3.0.0\"\n  }\n}\n",
    );
    repo.write(
        "src/schema.ts",
        "import { z } from \"zod\";\n\nexport const S = z.string();\n",
    );
    let binds = b5(&repo);
    assert!(
        binds.contains(&("src/schema.ts".into(), "package.json".into())),
        "{binds:?}"
    );
}

#[test]
fn manifest_removed_npm_dependency_binds_last_import_removal() {
    let repo = TestRepo::new();
    repo.write("package.json", "{\n  \"dependencies\": {\n    \"axios\": \"^1.7.0\",\n    \"left-pad\": \"1.0.0\"\n  }\n}\n");
    repo.write(
        "src/client.ts",
        "import axios from \"axios\";\n\nexport const get = () => axios.get(\"/x\");\n",
    );
    repo.commit_all("base");
    repo.write(
        "package.json",
        "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n",
    );
    repo.write("src/client.ts", "export const get = () => fetch(\"/x\");\n");
    let binds = b5(&repo);
    assert!(
        binds.contains(&("src/client.ts".into(), "package.json".into())),
        "{binds:?}"
    );
}

#[test]
fn manifest_cargo_dependency_binds_use() {
    let repo = TestRepo::new();
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\nanyhow = \"1\"\n",
    );
    repo.write("src/lib.rs", "pub fn f() -> u32 {\n    1\n}\n");
    repo.commit_all("base");
    repo.write("Cargo.toml", "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[dependencies]\nanyhow = \"1\"\nserde-json = \"1\"\n");
    repo.write("src/lib.rs", "pub fn f() -> u32 {\n    1\n}\n\npub fn g() -> String {\n    serde_json::to_string(&1).unwrap()\n}\n");
    let binds = b5(&repo);
    assert!(
        binds.contains(&("src/lib.rs".into(), "Cargo.toml".into())),
        "{binds:?}"
    );
}

#[test]
fn manifest_go_require_binds_import() {
    let repo = TestRepo::new();
    repo.write("go.mod", "module example.com/x\n\ngo 1.22\n");
    repo.write("main.go", "package main\n\nfunc main() {}\n");
    repo.commit_all("base");
    repo.write(
        "go.mod",
        "module example.com/x\n\ngo 1.22\n\nrequire github.com/spf13/cobra v1.8.0\n",
    );
    repo.write("main.go", "package main\n\nimport \"github.com/spf13/cobra\"\n\nfunc main() { _ = cobra.Command{} }\n");
    let binds = b5(&repo);
    assert!(
        binds.contains(&("main.go".into(), "go.mod".into())),
        "{binds:?}"
    );
}

#[test]
fn manifest_rule_is_inert_without_manifest_change() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        "{\n  \"dependencies\": {\n    \"axios\": \"^1.7.0\"\n  }\n}\n",
    );
    repo.write("src/client.ts", "export const get = () => 1;\n");
    repo.commit_all("base");
    repo.write(
        "src/client.ts",
        "import axios from \"axios\";\n\nexport const get = () => axios.get(\"/x\");\n",
    );
    assert!(b5(&repo).is_empty());
}

/// D48 review: a `scripts` key with the dependency's name, above `devDependencies`,
/// must not hide the dependency's line (B5 found no manifest atom and bound nothing).
#[test]
fn manifest_scripts_key_named_like_the_dependency_still_binds() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        "{\n  \"scripts\": {\n    \"vitest\": \"vitest run\"\n  },\n  \"devDependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n",
    );
    repo.write("src/a.test.ts", "export const a = 1;\n");
    repo.commit_all("base");
    repo.write(
        "package.json",
        "{\n  \"scripts\": {\n    \"vitest\": \"vitest run\"\n  },\n  \"devDependencies\": {\n    \"left-pad\": \"1.0.0\",\n    \"vitest\": \"^2.0.0\"\n  }\n}\n",
    );
    repo.write(
        "src/a.test.ts",
        "import { expect } from \"vitest\";\n\nexport const a = 1;\nexpect(a).toBe(1);\n",
    );
    let binds = b5(&repo);
    assert!(
        binds.contains(&("src/a.test.ts".into(), "package.json".into())),
        "{binds:?}"
    );
}

/// D48 review: a comment that names the dependency is not a use of it.
#[test]
fn manifest_comment_naming_the_dependency_does_not_bind() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        "{\n  \"dependencies\": {\n    \"left-pad\": \"1.0.0\"\n  }\n}\n",
    );
    repo.write("src/other.ts", "export function f() {\n  return 1;\n}\n");
    repo.commit_all("base");
    repo.write(
        "package.json",
        "{\n  \"dependencies\": {\n    \"axios\": \"^1.7.0\",\n    \"left-pad\": \"1.0.0\"\n  }\n}\n",
    );
    repo.write(
        "src/other.ts",
        "export function f() {\n  // migrated away from \"axios\" to fetch\n  return 1;\n}\n",
    );
    assert!(b5(&repo).is_empty(), "{:?}", b5(&repo));
}
