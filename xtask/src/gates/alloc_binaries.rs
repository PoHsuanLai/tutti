//! `check-alloc-binaries`: an allocation test is a module of its area's test
//! binary, not a binary of its own.
//!
//! `assert_no_alloc::AllocDisabler` does nothing outside an `assert_no_alloc`
//! closure, so an area binary (`tests/<area>/main.rs`) installs it once and its
//! allocation tests are modules in it. A file directly under a crate's `tests/`
//! is a binary of its own, and one that installs `AllocDisabler` as its
//! `#[global_allocator]` is a finding. A file with another allocator (a counting
//! or a no-free one) is not, and nor is a file in a subdirectory.
//!
//! [`ALLOWED`] is the debt, one row per file with what removes it. A row whose
//! file no longer installs `AllocDisabler` is an error, so the list cannot rot.
//!
//! Limits: a textual scan of code (comments and literals blanked). It sees a
//! `#[global_allocator]` whose `static` item names `AllocDisabler` before its
//! `;`.

use super::lex::{blank, find_word_from, line_of};
use super::{Report, Tree};

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// `(file, what removes the row)`.
pub(super) const ALLOWED: &[(&str, &str)] = &[
    (
        "crates/tutti-au-host/tests/au_offline.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-au-host/tests/au_process_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-clap-host/tests/clap_audio_thread_claim_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-clap-host/tests/clap_ports_rescan.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-clap-host/tests/clap_process_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-clap-host/tests/clap_thread_pool.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-clap-host/tests/clap_traits.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-plugin-host/tests/chunked_node.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-plugin-host/tests/vst2_in_process_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-plugin-host/tests/vst2_in_process_retire.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-plugin/tests/clap_node_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-plugin/tests/plugin_param_delivery_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-vst2-host/tests/vst2_host_process_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-vst2-host/tests/vst2_traits.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-vst3-host/tests/vst3_process_no_alloc.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
    (
        "crates/tutti-vst3-host/tests/vst3_traits.rs",
        "temporary: tutti-plugins folds it into its area binary",
    ),
];

/// Whether `path` is `crates/<crate>/tests/<file>.rs`, a test binary of its own.
fn is_top_level_test(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("crates/") else {
        return false;
    };
    let mut parts = rest.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(_), Some("tests"), Some(file), None, None) if file.ends_with(".rs")
    )
}

/// The line of the first `#[global_allocator]` whose static names `AllocDisabler`.
fn installs_alloc_disabler(text: &str) -> Option<usize> {
    let code = blank(text);
    let mut from = 0;
    while let Some(at) = code[from..].find("#[global_allocator]") {
        let start = from + at;
        let item = &code[start..];
        let end = item.find(';').unwrap_or(item.len());
        if find_word_from(&item[..end], "AllocDisabler", 0).is_some() {
            return Some(line_of(&code, start));
        }
        from = start + 1;
    }
    None
}

pub(super) fn check(tree: &dyn Tree, allowed: &[(&str, &str)]) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for path in tree.files("crates", &skipped) {
        if !is_top_level_test(&path) {
            continue;
        }
        let Some(text) = tree.read(&path) else {
            continue;
        };
        let Some(line) = installs_alloc_disabler(&text) else {
            continue;
        };
        match allowed.iter().find(|(p, _)| *p == path) {
            Some((p, _)) => seen.push(p),
            None => bad.push(format!(
                "{path}:{line}: installs AllocDisabler as its own test binary; make it a module of its \
                 area binary (tests/<area>/main.rs installs it once)"
            )),
        }
    }
    for (p, why) in allowed {
        if !seen.contains(p) {
            bad.push(format!("stale allowance {p} ({why})"));
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!("check-alloc-binaries: ok ({} allowed)", seen.len()));
    } else {
        r.fail("check-alloc-binaries: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const DISABLER: &str = "use assert_no_alloc::AllocDisabler;\n\n#[global_allocator]\nstatic A: AllocDisabler = AllocDisabler;\n";
    const COUNTING: &str = "#[global_allocator]\nstatic A: Counting = Counting;\n";

    fn run(files: &[(&str, &str)], allowed: &[(&str, &str)]) -> Report {
        check(&Mem::with(files), allowed)
    }

    #[test]
    fn a_top_level_test_file_installing_alloc_disabler_fails() {
        let r = run(&[("crates/tutti-a/tests/rt_no_alloc.rs", DISABLER)], &[]);
        assert_eq!(
            r.findings,
            [
                "check-alloc-binaries: FAIL",
                "  crates/tutti-a/tests/rt_no_alloc.rs:3: installs AllocDisabler as its own test binary; make it a module of its \
                 area binary (tests/<area>/main.rs installs it once)",
            ]
        );
    }

    #[test]
    fn an_area_binary_a_counting_allocator_and_a_comment_pass() {
        let r = run(
            &[
                ("crates/tutti-a/tests/area/main.rs", DISABLER),
                ("crates/tutti-a/tests/alloc_budget.rs", COUNTING),
                (
                    "crates/tutti-a/tests/prose.rs",
                    "// #[global_allocator]\n// static A: AllocDisabler = AllocDisabler;\n",
                ),
            ],
            &[],
        );
        assert_eq!(r.out, ["check-alloc-binaries: ok (0 allowed)"]);
        assert!(r.passed());
    }

    #[test]
    fn an_allowed_file_passes_and_a_stale_row_fails() {
        let allowed = [("crates/tutti-a/tests/old.rs", "temporary: x folds it")];
        let r = run(&[("crates/tutti-a/tests/old.rs", DISABLER)], &allowed);
        assert_eq!(r.out, ["check-alloc-binaries: ok (1 allowed)"]);

        let r = run(&[("crates/tutti-a/tests/old.rs", COUNTING)], &allowed);
        assert_eq!(
            r.findings,
            [
                "check-alloc-binaries: FAIL",
                "  stale allowance crates/tutti-a/tests/old.rs (temporary: x folds it)",
            ]
        );
    }
}
