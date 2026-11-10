//! Planning tests. Mutation record (each planted, seen to fail the named test,
//! restored):
//! - `dep` without the `done` check: `a_dependency_asked_for_twice_runs_once`;
//! - `plugin-probe` dep removed from `test`: `test_builds_the_plugins_then_runs_nextest`;
//! - `bench-save` passing `--baseline`: `bench_save_and_cmp_name_the_baseline`;
//! - the loom flag dropped from `check-loom`: `loom_recipes_carry_their_environment`;
//! - the stub moved back to `/tmp`: `check_windows_stubs_under_the_target_dir`;
//! - the `-E` filter dropped from `test`: `test_builds_the_plugins_then_runs_nextest`;
//! - `deny` without its probe: `deny_probes_cargo_deny_first`.
//! - `check-hosting` dropped from `ci`: `check_hosting_runs_its_script_and_ci_runs_it`.

use super::*;

fn ctx() -> Ctx {
    Ctx {
        root: "/repo".into(),
        target: "/repo/target".into(),
    }
}

fn dry(name: &str, args: &[&str]) -> Vec<String> {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    plan(name, &args, &ctx())
        .unwrap()
        .iter()
        .map(Step::display)
        .collect()
}

#[test]
fn every_recipe_plans_with_no_arguments() {
    for (name, ..) in RECIPES {
        assert!(!dry(name, &[]).is_empty(), "{name}");
    }
}

/// The justfile's 37, and `check-hosting` (design 021 step 1) since.
#[test]
fn the_justfiles_37_recipes_and_check_hosting() {
    assert_eq!(RECIPES.len(), 38);
}

#[test]
fn check_hosting_runs_its_script_and_ci_runs_it() {
    assert_eq!(dry("check-hosting", &[]), ["scripts/check-hosting.sh"]);
    assert!(
        dry("ci", &[])
            .iter()
            .any(|l| l == "scripts/check-hosting.sh")
    );
}

#[test]
fn test_builds_the_plugins_then_runs_nextest() {
    assert_eq!(
        dry("test", &["--no-fail-fast"]),
        [
            "cargo build --locked -p tutti-plugin-server --bin plugin-server",
            "cargo build --locked -p tutti-plugin-probe --bin plugin-probe",
            "cargo nextest run --locked --workspace --no-fail-fast -E 'not binary(gui_lifecycle_main) and not binary(au_gui_lifecycle_main)'",
        ]
    );
}

#[test]
fn ci_runs_each_dependency_once() {
    let steps = dry("ci", &[]);
    let count = |s: &str| steps.iter().filter(|l| l.as_str() == s).count();
    assert_eq!(
        count("cargo build --locked -p tutti-plugin-server --bin plugin-server"),
        1
    );
    assert_eq!(count("cargo fmt --all --check"), 1);
    // lint first, doc last, as the justfile's dependency list ran.
    assert_eq!(steps[0], "cargo fmt --all --check");
    assert!(steps.last().unwrap().contains("--cfg docsrs"));
    let deny = steps
        .iter()
        .position(|l| l.starts_with("cargo deny --version"))
        .unwrap();
    let loom = steps.iter().position(|l| l.contains("--cfg loom")).unwrap();
    let test = steps
        .iter()
        .position(|l| l.contains("nextest run --locked --workspace"))
        .unwrap();
    assert!(deny < loom && loom < test);
}

#[test]
fn test_all_does_not_repeat_the_plugin_builds() {
    let steps = dry("test-all", &[]);
    let n = steps
        .iter()
        .filter(|l| l.contains("--bin plugin-probe"))
        .count();
    assert_eq!(n, 1);
}

#[test]
fn bench_save_and_cmp_name_the_baseline() {
    let save = dry("bench-save", &["x"]);
    assert_eq!(save.len(), 11);
    assert!(save.iter().all(|l| l.ends_with("-- --save-baseline x")));
    let cmp = dry("bench-cmp", &[]);
    assert!(cmp.iter().all(|l| l.ends_with("-- --baseline main")));
}

#[test]
fn bench_smoke_runs_the_benches_then_the_comparison() {
    let s = dry("bench-smoke", &[]);
    assert_eq!(s.len(), 12);
    assert!(s[..11].iter().all(|l| l.ends_with("-- --test")));
    assert_eq!(
        s[11],
        "cargo bench --locked -p arc-lend-compare --bench compare -- --test"
    );
}

#[test]
fn loom_recipes_carry_their_environment() {
    let s = dry("loom", &[]);
    assert_eq!(
        s[1],
        "LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS='--cfg loom' cargo test --locked -p arc-lend --release --test loom -- registering_and_dropping_readers --exact"
    );
    assert_eq!(
        dry("check-loom", &[]),
        [
            "RUSTFLAGS='--cfg loom' cargo clippy --locked -p arc-lend -p tutti-rt -p tutti-shm-model --all-targets -- -D warnings"
        ]
    );
}

#[test]
fn only_the_first_editor_line_takes_the_skip_variable() {
    let s = dry("test-editor-no-corpus", &[]);
    assert!(s[0].starts_with("TUTTI_GUI_SKIP=has_editor_agrees_with_opening_one cargo test"));
    assert!(s[1].starts_with("cargo test"));
}

#[test]
fn check_windows_stubs_under_the_target_dir() {
    let steps = plan("check-windows", &[], &ctx()).unwrap();
    assert_eq!(steps.len(), 10);
    let Step::WriteFile { path, contents } = &steps[0] else {
        panic!()
    };
    assert_eq!(path, &PathBuf::from("/repo/target/tutti-winstub/malloc.h"));
    assert_eq!(contents, "#include <stddef.h>\nvoid *_alloca(size_t);\n");
    for s in &steps[1..] {
        let l = s.display();
        assert!(l.starts_with(
            "CC_x86_64_pc_windows_msvc=clang AR_x86_64_pc_windows_msvc=llvm-ar CFLAGS_x86_64_pc_windows_msvc=-I/repo/target/tutti-winstub cargo clippy --locked --target x86_64-pc-windows-msvc "
        ), "{l}");
        assert!(l.ends_with(" -- -D warnings"));
    }
}

#[test]
fn deny_probes_cargo_deny_first() {
    let steps = plan("deny", &[], &ctx()).unwrap();
    assert_eq!(steps[0], Step::RequireCargoDeny);
    assert_eq!(steps[1].display(), "cargo deny --locked check advisories");
}

#[test]
fn doc_sets_rustdocflags_and_the_dated_nightly() {
    let s = dry("doc", &[]);
    assert_eq!(s.len(), 4);
    assert!(s[0].starts_with("RUSTDOCFLAGS='-D warnings --document-private-items' cargo doc"));
    assert_eq!(s[2], "scripts/check-doc-features.sh");
    assert!(s[3].contains("cargo +nightly-2026-10-14 doc"));
}

#[test]
fn profile_bench_keeps_its_defaults_and_its_glob() {
    let s = dry("profile-bench", &[]);
    assert_eq!(
        s[0],
        "cargo bench --locked -p tutti-nodes --bench engine_render --profile profiling --no-run"
    );
    assert_eq!(
        s[1],
        "samply record target/profiling/deps/engine_render-* --bench --profile-time 10"
    );
}

#[test]
fn verify_audio_defaults_its_out_dir() {
    let s = dry("verify-audio", &[]);
    assert_eq!(s.len(), 9);
    assert!(s[0].ends_with("render_cases -- /tmp/tutti-verify/sampler"));
    assert!(dry("verify-audio", &["o"])[8].ends_with("verify_synth.py o/synth"));
}

#[test]
fn arguments_are_refused_where_the_recipe_has_none() {
    assert!(plan("lint", &["x".into()], &ctx()).is_err());
    assert!(plan("bench-save", &["a".into(), "b".into()], &ctx()).is_err());
    assert!(plan("nope", &[], &ctx()).is_err());
}

#[test]
fn check_paths_and_doc_tests_pass_their_arguments_through() {
    assert_eq!(
        dry("check-paths", &["--fix"]),
        ["scripts/check-canonical-paths.sh --fix"]
    );
    let d = dry("test-doc", &["--", "x"]);
    assert_eq!(d[0], "cargo test --locked --doc --workspace -- x");
    assert_eq!(
        d[1],
        "cargo test --locked --doc -p tutti-session --features device -- x"
    );
}

#[test]
fn a_dependency_asked_for_twice_runs_once() {
    let c = ctx();
    let mut p = Planner {
        ctx: &c,
        done: HashSet::new(),
        out: Vec::new(),
    };
    p.dep("plugin-probe").unwrap();
    p.dep("plugin-probe").unwrap();
    assert_eq!(p.out.len(), 1);
}
