//! M2 acceptance (SPEC §12): one fixture per grouping rule, plus the D5, D8, D9, and D10
//! behaviors, determinism, and a full carve of the demo repository.

mod common;

use common::{TestRepo, atoms_in, group_of};
use git_carve::atom::AtomId;
use git_carve::commit::Hooks;
use git_carve::graph::Tag;

fn one_group(plan: &git_carve::graph::Plan, ids: &[AtomId]) -> usize {
    let g = group_of(plan, ids[0]).expect("grouped");
    for &id in ids {
        assert_eq!(group_of(plan, id), Some(g), "{id} is not with {}", ids[0]);
    }
    g
}

#[test]
fn demo_repo_is_three_commits_and_two_leftovers() {
    let repo = TestRepo::demo();
    let (changes, _, plan) = repo.plan();
    assert_eq!(plan.groups.len(), 3, "{:?}", plan.groups);
    assert_eq!(plan.leave.len(), 2);

    // Rename across 6 files, with each file's import line.
    let rename: Vec<AtomId> = [
        "src/format.ts",
        "src/report.ts",
        "src/table.ts",
        "src/progress.ts",
        "src/summary.ts",
    ]
    .iter()
    .flat_map(|p| atoms_in(&changes, p))
    .collect();
    let main = atoms_in(&changes, "src/main.ts");
    let rename_group = one_group(&plan, &[rename.clone(), vec![main[0], main[2]]].concat());

    // Feature: new function, call site, test, and the test's import line.
    let net = atoms_in(&changes, "src/net.ts");
    let test = atoms_in(&changes, "tests/net.test.ts");
    let feature = one_group(&plan, &[net[0], net[2], test[0], test[1]]);

    // Drive-by fix, alone.
    let fix = atoms_in(&changes, "src/parse.ts");
    let fix_group = one_group(&plan, &fix);
    assert_eq!(plan.groups[fix_group].len(), 1);
    assert!(rename_group != feature && feature != fix_group && rename_group != fix_group);

    // Both debug lines left out and tagged.
    assert_eq!(plan.leave, vec![main[1], net[1]]);
    for id in &plan.leave {
        assert!(plan.tags[id].contains(&Tag::DebugOnly));
    }
}

#[test]
fn plans_are_deterministic_across_runs_and_locations() {
    let (a, b) = (TestRepo::demo(), TestRepo::demo());
    let json = |r: &TestRepo| {
        let (changes, analysis, plan) = r.plan();
        git_carve::report::json(&changes, &analysis, &plan, None)
    };
    let first = json(&a);
    assert_eq!(first, json(&a), "same repo, same plan");
    assert_eq!(first, json(&b), "same content elsewhere, same plan");
}

#[test]
fn demo_carve_matches_oracle_outside_leftovers() {
    let repo = TestRepo::demo();
    let before = repo.worktree_digest();
    let oracle = repo.oracle_tree();
    let (_, _, plan) = repo.plan();
    let groups = plan.groups.clone();
    let record = repo.carve_lib(|_| groups, &Hooks::default()).unwrap();
    assert_eq!(record.commits.len(), 3);
    assert_eq!(repo.worktree_digest(), before);
    let differing = repo.run(&["diff-tree", "-r", "--name-only", &repo.head_tree(), &oracle]);
    assert_eq!(
        differing, "src/main.ts\nsrc/net.ts",
        "only files with debug lines differ"
    );
    assert_eq!(
        repo.run(&["diff", "--stat"]).lines().count(),
        3,
        "two files plus the summary"
    );
}

#[test]
fn drive_by_fix_in_an_unrelated_function_is_separate() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export function alpha(n: number) {\n  return n + 1;\n}\n\nexport function beta(n: number) {\n  return n * 2;\n}\n");
    repo.commit_all("base");
    repo.write("src/a.ts", "export function alpha(n: number) {\n  return n + 2;\n}\n\nexport function beta(n: number) {\n  return n * 3;\n}\n");
    let (_, _, plan) = repo.plan();
    assert_eq!(plan.groups.len(), 2);
}

#[test]
fn edits_in_one_function_are_one_group() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export function alpha(n: number) {\n  const a = n + 1;\n  const b = a * 2;\n  const c = b - 3;\n  return c;\n}\n");
    repo.commit_all("base");
    repo.write("src/a.ts", "export function alpha(n: number) {\n  const a = n + 10;\n  const b = a * 2;\n  const c = b - 30;\n  return c;\n}\n");
    let (changes, _, plan) = repo.plan();
    assert_eq!(changes.atoms.len(), 2);
    assert_eq!(plan.groups.len(), 1, "F1: same enclosing function");
}

#[test]
fn whitespace_only_change_goes_to_a_trailing_style_group() {
    let repo = TestRepo::new();
    repo.write(
        "src/a.ts",
        "export function alpha() {\n  return 1;\n}\n\nexport function beta() {\n  return 2;\n}\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/a.ts",
        "export function alpha() {\n    return 1;\n}\n\nexport function beta() {\n  return 3;\n}\n",
    );
    let (changes, _, plan) = repo.plan();
    let style = plan.style.expect("style group");
    assert_eq!(style, plan.groups.len() - 1, "style comes last");
    assert_eq!(plan.groups[style], vec![changes.atoms[0].id]);
    assert!(plan.tags[&changes.atoms[0].id].contains(&Tag::Style));
}

#[test]
fn debug_line_in_a_file_that_already_logs_is_kept_and_tagged() {
    let repo = TestRepo::new();
    repo.write("src/server.ts", "export function start(port: number) {\n  console.log(\"starting\");\n  listen(port);\n}\n\nfunction listen(p: number) {\n  return p;\n}\n");
    repo.commit_all("base");
    repo.write("src/server.ts", "export function start(port: number) {\n  console.log(\"starting\");\n  listen(port);\n}\n\nfunction listen(p: number) {\n  console.log(\"listening on\", p);\n  return p;\n}\n");
    let (changes, _, plan) = repo.plan();
    let id = changes.atoms[0].id;
    assert!(
        plan.leave.is_empty(),
        "D8: this file already uses console.log"
    );
    assert!(plan.tags[&id].contains(&Tag::LooksLikeDebug));
}

#[test]
fn debugger_and_new_console_calls_are_left_but_console_error_is_not() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export function a() {\n  return 1;\n}\n\nexport function b() {\n  return 2;\n}\n\nexport function c() {\n  return 3;\n}\n");
    repo.commit_all("base");
    repo.write("src/a.ts", "export function a() {\n  debugger;\n  return 1;\n}\n\nexport function b() {\n  console.error(\"real\");\n  return 2;\n}\n\nexport function c() {\n  console.table([1]);\n  return 3;\n}\n");
    let (changes, _, plan) = repo.plan();
    let ids: Vec<AtomId> = changes.atoms.iter().map(|a| a.id).collect();
    assert_eq!(plan.leave, vec![ids[0], ids[2]]);
    assert!(
        group_of(&plan, ids[1]).is_some(),
        "console.error is intentional"
    );
}

#[test]
fn manifest_and_lockfile_commit_together() {
    let repo = TestRepo::new();
    repo.write(
        "package.json",
        "{\n  \"name\": \"x\",\n  \"dependencies\": {}\n}\n",
    );
    repo.write("package-lock.json", "{\n  \"lockfileVersion\": 3\n}\n");
    repo.write("src/a.ts", "export const a = 1;\n");
    repo.commit_all("base");
    repo.write(
        "package.json",
        "{\n  \"name\": \"x\",\n  \"dependencies\": { \"zod\": \"^3\" }\n}\n",
    );
    repo.write(
        "package-lock.json",
        "{\n  \"lockfileVersion\": 3,\n  \"packages\": {}\n}\n",
    );
    repo.write("src/a.ts", "export const a = 2;\n");
    let (changes, _, plan) = repo.plan();
    one_group(
        &plan,
        &[
            atoms_in(&changes, "package.json"),
            atoms_in(&changes, "package-lock.json"),
        ]
        .concat(),
    );
    assert_eq!(plan.groups.len(), 2);
}

#[test]
fn mode_change_travels_with_content() {
    let repo = TestRepo::new();
    repo.write(
        "bin/run.ts",
        "export function one() {\n  return 1;\n}\n\nexport function two() {\n  return 2;\n}\n",
    );
    repo.commit_all("base");
    repo.write(
        "bin/run.ts",
        "export function one() {\n  return 10;\n}\n\nexport function two() {\n  return 20;\n}\n",
    );
    repo.set_exec("bin/run.ts", true);
    let (changes, _, plan) = repo.plan();
    assert_eq!(changes.atoms.len(), 3);
    assert_eq!(
        plan.groups.len(),
        1,
        "B4 binds the mode atom to every content atom"
    );
}

#[test]
fn import_line_shared_by_two_features_is_reported_as_bridging() {
    let repo = TestRepo::new();
    // Two features, each adding a helper in its own file (in one file, D41's same-file rule
    // would join them) and using it from its own function of app.ts.
    repo.write("src/util.ts", "export function base() {\n  return 0;\n}\n");
    repo.write("src/app.ts", "import { base } from \"./util\";\n\nexport function first() {\n  return base();\n}\n\nexport function second() {\n  return base();\n}\n");
    repo.commit_all("base");
    repo.write(
        "src/util.ts",
        "export function retryIt() {\n  return 1;\n}\n\nexport function base() {\n  return 0;\n}\n",
    );
    repo.write(
        "src/clamp.ts",
        "export function clampIt() {\n  return 2;\n}\n",
    );
    repo.write("src/app.ts", "import { base, retryIt } from \"./util\";\nimport { clampIt } from \"./clamp\";\n\nexport function first() {\n  return base() + retryIt();\n}\n\nexport function second() {\n  return base() + clampIt();\n}\n");
    let (changes, _, plan) = repo.plan();
    let app = atoms_in(&changes, "src/app.ts");
    let util = atoms_in(&changes, "src/util.ts");
    let clamp = atoms_in(&changes, "src/clamp.ts");
    assert_eq!(plan.groups.len(), 2, "{:?}", plan.groups);
    let g1 = one_group(&plan, &[util[0], app[1]]);
    let g2 = one_group(&plan, &[clamp[0], app[2]]);
    assert_ne!(g1, g2);
    assert_eq!(plan.diagnostics.bridging_import_atoms, vec![app[0]], "D5");
    assert_eq!(
        group_of(&plan, app[0]),
        Some(g1.min(g2)),
        "placed in the earliest user"
    );
}

#[test]
fn symbolless_files_attach_by_stem_or_stand_alone() {
    let repo = TestRepo::new();
    repo.write(
        "src/Button.tsx",
        "export function Button() {\n  return 1;\n}\n\nexport function Other() {\n  return 2;\n}\n",
    );
    repo.write("src/Button.module.css", ".button { color: red; }\n");
    repo.write(
        "src/__snapshots__/Button.test.tsx.snap",
        "exports[`Button`] = `1`;\n",
    );
    repo.write("README.md", "# demo\n");
    repo.commit_all("base");
    repo.write("src/Button.tsx", "export function Button() {\n  return 10;\n}\n\nexport function Other() {\n  return 20;\n}\n");
    repo.write("src/Button.module.css", ".button { color: blue; }\n");
    repo.write(
        "src/__snapshots__/Button.test.tsx.snap",
        "exports[`Button`] = `10`;\n",
    );
    repo.write("README.md", "# demo\n\nMore docs.\n");
    let (changes, _, plan) = repo.plan();
    let button = atoms_in(&changes, "src/Button.tsx");
    let css = atoms_in(&changes, "src/Button.module.css")[0];
    let snap = atoms_in(&changes, "src/__snapshots__/Button.test.tsx.snap")[0];
    let readme = atoms_in(&changes, "README.md")[0];
    let css_group = group_of(&plan, css).unwrap();
    assert!(
        button
            .iter()
            .any(|&b| group_of(&plan, b) == Some(css_group)),
        "D9: CSS joins Button.tsx"
    );
    assert_eq!(
        group_of(&plan, snap),
        Some(css_group),
        "snapshot joins the same group"
    );
    assert_eq!(plan.groups[group_of(&plan, readme).unwrap()], vec![readme]);
    assert_eq!(plan.diagnostics.orphan_groups, 1);
}

#[test]
fn same_name_in_unrelated_files_does_not_link() {
    let repo = TestRepo::new();
    repo.write(
        "src/a.ts",
        "export function formatValue(n: number) {\n  return `${n}`;\n}\n",
    );
    repo.write(
        "src/b.ts",
        "export function formatValue(n: number) {\n  return n.toFixed(2);\n}\n",
    );
    repo.write(
        "src/c.ts",
        "import { formatValue } from \"./a\";\n\nexport function show() {\n  return 0;\n}\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/a.ts",
        "export function formatValue(n: number) {\n  return `${n}!`;\n}\n",
    );
    repo.write(
        "src/b.ts",
        "export function formatValue(n: number) {\n  return n.toFixed(3);\n}\n",
    );
    repo.write("src/c.ts", "import { formatValue } from \"./a\";\n\nexport function show() {\n  return formatValue(1);\n}\n");
    let (changes, _, plan) = repo.plan();
    let a = atoms_in(&changes, "src/a.ts")[0];
    let b = atoms_in(&changes, "src/b.ts")[0];
    let c = atoms_in(&changes, "src/c.ts")[0];
    assert_eq!(
        group_of(&plan, a),
        group_of(&plan, c),
        "F3 through the import of ./a"
    );
    assert_ne!(
        group_of(&plan, b),
        group_of(&plan, c),
        "D10: b's formatValue is not imported"
    );
}

#[test]
fn tsconfig_path_alias_resolves_to_the_changed_file() {
    let repo = TestRepo::new();
    repo.write("tsconfig.json", "{\n  // comments are allowed\n  \"compilerOptions\": { \"paths\": { \"@/*\": [\"./src/*\"] } },\n}\n");
    repo.write("src/lib/helper.ts", "export const version = 1;\n");
    repo.write("src/app/page.tsx", "export function Page() {\n  return null;\n}\n\nexport function Footer() {\n  return 1;\n}\n");
    repo.commit_all("base");
    repo.write("src/lib/helper.ts", "export const version = 1;\n\nexport function computeTotal(xs: number[]) {\n  return xs.length;\n}\n");
    repo.write("src/app/page.tsx", "import { computeTotal } from \"@/lib/helper\";\n\nexport function Page() {\n  return computeTotal([1, 2]);\n}\n\nexport function Footer() {\n  return 2;\n}\n");
    let (changes, _, plan) = repo.plan();
    let helper = atoms_in(&changes, "src/lib/helper.ts")[0];
    let page = atoms_in(&changes, "src/app/page.tsx");
    one_group(&plan, &[helper, page[0], page[1]]);
    assert_ne!(
        group_of(&plan, page[2]),
        group_of(&plan, helper),
        "Footer's edit is unrelated"
    );
}

#[test]
fn new_helper_stays_with_many_call_sites() {
    let repo = TestRepo::new();
    let mut base = String::from("export function start() {\n  return 0;\n}\n");
    for i in 0..5 {
        base.push_str(&format!(
            "\nexport function site{i}() {{\n  return {i};\n}}\n"
        ));
    }
    repo.write("src/a.ts", &base);
    repo.commit_all("base");
    let mut new = String::from(
        "export function clampNumber(n: number) {\n  return Math.max(0, n);\n}\n\nexport function start() {\n  return 0;\n}\n",
    );
    for i in 0..5 {
        new.push_str(&format!(
            "\nexport function site{i}() {{\n  return clampNumber({i});\n}}\n"
        ));
    }
    repo.write("src/a.ts", &new);
    let (changes, _, plan) = repo.plan();
    assert_eq!(changes.atoms.len(), 6);
    assert_eq!(plan.groups.len(), 1, "F0 has no rarity cap (D10)");
}

#[test]
fn a_shared_rare_name_links_again_after_d26() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export function first() {\n  return 0;\n}\n");
    repo.write("src/b.ts", "export function second() {\n  return 0;\n}\n");
    repo.commit_all("base");
    repo.write(
        "src/a.ts",
        "export function first() {\n  return quotaLimit;\n}\n",
    );
    repo.write(
        "src/b.ts",
        "export function second() {\n  return quotaLimit;\n}\n",
    );
    let (changes, _, plan) = repo.plan();
    let (a, b) = (changes.atoms[0].id, changes.atoms[1].id);
    assert_eq!(
        group_of(&plan, a),
        group_of(&plan, b),
        "D26: F2 merges in code-only mode again"
    );
    assert!(
        plan.edges
            .iter()
            .any(|(x, y, kinds)| (*x, *y) == (a, b) && kinds.contains(&"F2")),
        "{:?}",
        plan.edges
    );
}

#[test]
fn systematic_edits_link_across_files() {
    let repo = TestRepo::new();
    repo.write(
        "src/a.ts",
        "export function one() {\n  return legacyFetch(1);\n}\n",
    );
    repo.write(
        "mirror/a.ts",
        "export function one() {\n  return legacyFetch(1);\n}\n",
    );
    repo.write(
        "src/b.ts",
        "export function two(x: number) {\n  return oldClient.get(x);\n}\n",
    );
    repo.write(
        "src/c.ts",
        "export function three(y: string) {\n  return oldClient.get(y);\n}\n",
    );
    repo.commit_all("base");
    repo.write(
        "src/a.ts",
        "export function one() {\n  return modernFetch(1, { retry: true });\n}\n",
    );
    repo.write(
        "mirror/a.ts",
        "export function one() {\n  return modernFetch(1, { retry: true });\n}\n",
    );
    repo.write(
        "src/b.ts",
        "export function two(x: number) {\n  return newClient.get(x);\n}\n",
    );
    repo.write(
        "src/c.ts",
        "export function three(y: string) {\n  return newClient.get(y);\n}\n",
    );
    let (changes, _, plan) = repo.plan();
    let id = |p: &str| atoms_in(&changes, p)[0];
    assert_eq!(
        group_of(&plan, id("src/a.ts")),
        group_of(&plan, id("mirror/a.ts")),
        "identical edits (a mirror)"
    );
    assert_eq!(
        group_of(&plan, id("src/b.ts")),
        group_of(&plan, id("src/c.ts")),
        "the same substitution (a rename)"
    );
    assert_ne!(
        group_of(&plan, id("src/a.ts")),
        group_of(&plan, id("src/b.ts")),
        "different edits stay apart"
    );
    assert!(plan.diagnostics.soft_edges_by_kind.contains_key("F6"));
}

/// D41: edits in one file join, unless one is a drive-by shape (SPEC §12 M2), and the joins
/// that rest on the file alone are tagged so the user can see where to split.
#[test]
fn edits_in_one_file_join_and_say_so() {
    let repo = TestRepo::new();
    repo.write("src/a.ts", "export function alpha(n: number) {\n  return n + 1;\n}\n\nexport function beta(n: number) {\n  return n * 2;\n}\n");
    repo.commit_all("base");
    // Two new helpers in one file, used nowhere else yet: one feature, most of the time.
    repo.write("src/a.ts", "export function alpha(n: number) {\n  return n + 1;\n}\n\nexport function scaled(n: number) {\n  const k = 3;\n  return n * k;\n}\n\nexport function beta(n: number) {\n  return n * 2;\n}\n\nexport function shifted(n: number) {\n  const k = 4;\n  return n + k;\n}\n");
    let (changes, _, plan) = repo.plan();
    let atoms = atoms_in(&changes, "src/a.ts");
    assert_eq!(atoms.len(), 2);
    one_group(&plan, &atoms);
    assert_eq!(plan.groups.len(), 1);
    let tagged: Vec<AtomId> = atoms
        .iter()
        .copied()
        .filter(|id| {
            plan.tags
                .get(id)
                .is_some_and(|t| t.contains(&Tag::SameFile))
        })
        .collect();
    assert_eq!(tagged.len(), 1, "one side is joined by the file alone");
}

/// D42, found on zod's history: logging in an error path is error reporting, not a leftover
/// debug print, so it stays in its commit. A stray console.log beside it is still left out.
#[test]
fn logging_in_an_error_path_is_committed_not_left() {
    let repo = TestRepo::new();
    let body = |extra_catch: &str, extra_throw: &str, stray: &str| {
        format!(
            "export async function load(url: string) {{\n  try {{\n    const res = await fetch(url);\n    const json = await res.json();\n\n    if (json.errors) {{\n{extra_throw}      throw new Error(\"bad\");\n    }}\n\n    const count = json.count;\n{stray}    return count;\n  }} catch (e) {{\n{extra_catch}    throw new Error(\"failed\");\n  }}\n}}\n"
        )
    };
    repo.write("src/load.ts", body("", "", ""));
    repo.commit_all("base");
    repo.write(
        "src/load.ts",
        body(
            "    console.log(e);\n",
            "      console.dir(json.errors, { depth: null });\n",
            "\n    console.log(count);\n\n",
        ),
    );
    let (changes, _, plan) = repo.plan();
    let atoms = atoms_in(&changes, "src/load.ts");
    assert_eq!(atoms.len(), 3, "three separate one-line additions");
    let left: Vec<AtomId> = plan.leave.clone();
    assert_eq!(
        left.len(),
        1,
        "only the stray print is left: {:?}",
        plan.leave
    );
    let stray = left[0];
    assert!(
        changes.position(stray).starts_with("src/load.ts:1"),
        "{}",
        changes.position(stray)
    );
    for &id in atoms.iter().filter(|&&id| id != stray) {
        assert!(group_of(&plan, id).is_some(), "{id} is committed");
    }
}
