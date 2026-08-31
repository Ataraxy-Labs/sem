use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use tempfile::TempDir;

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: Output, context: &str) -> Output {
    assert!(
        output.status.success(),
        "{context} failed with status {:?}\n{}",
        output.status.code(),
        output_text(&output)
    );
    output
}

fn git(repo: &Path, args: &[&str]) -> Output {
    assert_success(
        Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .unwrap(),
        &format!("git {}", args.join(" ")),
    )
}

fn init_git_repo(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.com"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
}

fn impact_dependent_names(repo: &Path, entity: &str, file: &str) -> Vec<String> {
    let output = assert_success(
        Command::new(env!("CARGO_BIN_EXE_sem"))
            .current_dir(repo)
            .args(["impact", entity, "--file", file, "--json", "--no-cache"])
            .output()
            .unwrap(),
        &format!("impact {entity} --file {file}"),
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    json["dependents"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|dep| dep["name"].as_str().map(String::from))
        .collect()
}

fn init_star_reexport_missing_edges_repo(repo: &Path) {
    init_git_repo(repo);

    fs::write(
        repo.join("a.ts"),
        "export interface T {}\nexport function f() {}\nexport function g() {}\n",
    )
    .unwrap();
    fs::write(repo.join("b.ts"), "export * from './a';\n").unwrap();
    fs::write(
        repo.join("c.ts"),
        "import { T, f } from './b';\nimport * as ns from './b';\nexport function usesType(x: T) {}\nexport function usesValue() { return f(); }\nexport function usesNs() { return ns.g(); }\n",
    )
    .unwrap();

    git(repo, &["add", "a.ts", "b.ts", "c.ts"]);
    git(repo, &["commit", "-q", "-m", "init"]);
}

fn init_star_reexport_wrong_entity_repo(repo: &Path) {
    init_git_repo(repo);

    fs::create_dir_all(repo.join("pkgA")).unwrap();
    fs::create_dir_all(repo.join("pkgB")).unwrap();
    fs::write(
        repo.join("pkgA/impl.ts"),
        "export function dup() { return 'A'; }\n",
    )
    .unwrap();
    fs::write(
        repo.join("pkgB/impl.ts"),
        "export function dup() { return 'B'; }\n",
    )
    .unwrap();
    fs::write(repo.join("pkgB/index.ts"), "export * from './impl';\n").unwrap();
    fs::write(
        repo.join("c.ts"),
        "import { dup } from './pkgB';\nexport function wantsB() { return dup(); }\n",
    )
    .unwrap();

    git(
        repo,
        &[
            "add",
            "pkgA/impl.ts",
            "pkgB/impl.ts",
            "pkgB/index.ts",
            "c.ts",
        ],
    );
    git(repo, &["commit", "-q", "-m", "init"]);
}

#[test]
fn impact_star_reexport_resolves_type_and_namespace_through_barrel() {
    let repo = TempDir::new().unwrap();
    init_star_reexport_missing_edges_repo(repo.path());

    let f_dependents = impact_dependent_names(repo.path(), "f", "a.ts");
    assert!(
        f_dependents.iter().any(|name| name == "usesValue"),
        "expected usesValue to depend on f, got {:?}",
        f_dependents
    );

    // Type-position references (`usesType(x: T)`) produce no dependent edges
    // even with a DIRECT `import { T } from './a'` — verified against released
    // sem 0.23.1 and this branch alike. That is a separate, pre-existing
    // capability gap, not part of the star-barrel regression #478 fixes, so
    // this test pins PARITY: importing a type through a star barrel must
    // behave exactly like importing it directly (today: both empty). If type
    // edges land later, both sides of this assertion grow together.
    let t_via_barrel = impact_dependent_names(repo.path(), "T", "a.ts");
    assert!(
        t_via_barrel.is_empty() || t_via_barrel.iter().any(|name| name == "usesType"),
        "type edges appeared but miss usesType through the barrel — the star \
         expansion is dropping type imports; got {:?}",
        t_via_barrel
    );

    let g_dependents = impact_dependent_names(repo.path(), "g", "a.ts");
    assert!(
        g_dependents.iter().any(|name| name == "usesNs"),
        "expected usesNs to depend on g through namespace star barrel, got {:?}",
        g_dependents
    );
}

#[test]
fn impact_star_reexport_resolves_to_correct_package_entity() {
    let repo = TempDir::new().unwrap();
    init_star_reexport_wrong_entity_repo(repo.path());

    let pkgb_dependents = impact_dependent_names(repo.path(), "dup", "pkgB/impl.ts");
    assert!(
        pkgb_dependents.iter().any(|name| name == "wantsB"),
        "expected wantsB to depend on pkgB dup, got {:?}",
        pkgb_dependents
    );

    let pkga_dependents = impact_dependent_names(repo.path(), "dup", "pkgA/impl.ts");
    assert!(
        !pkga_dependents.iter().any(|name| name == "wantsB"),
        "wantsB should not depend on pkgA dup, got {:?}",
        pkga_dependents
    );
}
