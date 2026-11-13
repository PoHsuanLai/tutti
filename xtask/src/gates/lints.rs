//! `check-lints`: the workspace's lint table reaches every shipped crate.
//!
//! - **Every `crates/*/Cargo.toml` has `[lints]` with `workspace = true`**, except
//!   the crates in [`UNLINTED`], each with its reason. A listed crate that takes
//!   the table after all, or is gone, is a stale entry.
//! - **The root `[workspace.lints.rust]` sets `missing_debug_implementations` to
//!   `deny`**: a public type without `Debug` fails the build on its own, not
//!   only under `-D warnings`.

use super::toml::Toml;
use super::{Report, Tree};

/// `(crate directory, why it does not take the workspace lints)`.
pub(super) const UNLINTED: &[(&str, &str)] = &[];

const LINT: &str = "missing_debug_implementations";

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

fn takes_workspace_lints(manifest: &Toml) -> bool {
    manifest.value("lints", "workspace") == Some("true")
}

pub(super) fn check(tree: &dyn Tree, unlinted: &[(&str, &str)]) -> Report {
    let mut bad: Vec<String> = Vec::new();
    let files = tree.files("crates", &skipped);
    let mut crates: Vec<&str> = files
        .iter()
        .filter_map(|p| p.strip_prefix("crates/")?.strip_suffix("/Cargo.toml"))
        .filter(|n| !n.contains('/'))
        .collect();
    crates.sort_unstable();
    if crates.len() < 20 {
        bad.push(format!(
            "only {} crates found under crates/; the scan is broken",
            crates.len()
        ));
    }

    for name in &crates {
        let path = format!("crates/{name}/Cargo.toml");
        let Some(text) = tree.read(&path) else {
            continue;
        };
        let takes = takes_workspace_lints(&Toml::parse(&text));
        match unlinted.iter().find(|(c, _)| c == name) {
            None if !takes => bad.push(format!(
                "{path}: missing `[lints] workspace = true` (a crate that cannot take it goes in UNLINTED in xtask/src/gates/lints.rs, with its reason)"
            )),
            Some((_, why)) if takes => bad.push(format!(
                "{path}: takes the workspace lints but is listed in UNLINTED ({why}); remove the entry"
            )),
            _ => {}
        }
    }
    for (name, _) in unlinted {
        if !crates.contains(name) {
            bad.push(format!(
                "crates/{name}: listed in UNLINTED but not a crate; was it moved?"
            ));
        }
    }

    match tree.read("Cargo.toml") {
        None => bad.push("Cargo.toml: missing".to_owned()),
        Some(root) => {
            let root = Toml::parse(&root);
            let level = root
                .value("workspace.lints.rust", LINT)
                .map(|v| v.trim().trim_matches('"'));
            if level != Some("deny") {
                bad.push(format!(
                    "Cargo.toml: [workspace.lints.rust] must set {LINT} = \"deny\" (found {})",
                    level.unwrap_or("nothing")
                ));
            }
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!(
            "check-lints: ok ({} crates, {} unlinted by name)",
            crates.len(),
            unlinted.len()
        ));
    } else {
        r.fail("check-lints: FAIL");
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

    const ROOT: &str = "[workspace.lints.rust]\nmissing_docs = \"warn\"\nmissing_debug_implementations = \"deny\"\n";
    const LINTED: &str = "[package]\nname = \"x\"\n\n[lints]\nworkspace = true\n";

    fn clean() -> Mem {
        let mut m = Mem::default();
        m.files.insert("Cargo.toml".to_owned(), ROOT.to_owned());
        for i in 0..20 {
            m.files
                .insert(format!("crates/c{i}/Cargo.toml"), LINTED.to_owned());
        }
        m
    }

    fn with(mut m: Mem, path: &str, text: &str) -> Report {
        m.files.insert(path.to_owned(), text.to_owned());
        check(&m, &[])
    }

    #[test]
    fn a_clean_tree_passes() {
        let r = check(&clean(), &[]);
        assert!(r.passed(), "{:?}", r.findings);
        assert!(r.out[0].starts_with("check-lints: ok (20 crates"));
    }

    #[test]
    fn a_crate_without_the_workspace_lints_fails() {
        let r = with(clean(), "crates/c3/Cargo.toml", "[package]\nname = \"x\"\n");
        assert_eq!(r.findings[0], "check-lints: FAIL");
        assert!(r.findings[1].contains("crates/c3/Cargo.toml: missing `[lints] workspace = true`"));
    }

    #[test]
    fn a_lints_table_of_its_own_is_not_the_workspace_table() {
        let r = with(
            clean(),
            "crates/c3/Cargo.toml",
            "[lints.rust]\nmissing_docs = \"warn\"\n",
        );
        assert_eq!(r.findings.len(), 2);
    }

    #[test]
    fn a_workspace_table_that_only_warns_fails() {
        let r = with(
            clean(),
            "Cargo.toml",
            "[workspace.lints.rust]\nmissing_debug_implementations = \"warn\"\n",
        );
        assert!(r.findings[1].contains("must set missing_debug_implementations"));
        let r = with(clean(), "Cargo.toml", "[workspace.lints.rust]\n");
        assert!(r.findings[1].contains("found nothing"));
    }

    #[test]
    fn an_unlinted_entry_passes_while_the_crate_lacks_the_table() {
        let mut m = clean();
        m.files.insert(
            "crates/c3/Cargo.toml".to_owned(),
            "[package]\nname = \"x\"\n".to_owned(),
        );
        let r = check(&m, &[("c3", "needs its own table")]);
        assert!(r.passed(), "{:?}", r.findings);
    }

    #[test]
    fn a_stale_unlinted_entry_fails() {
        let r = check(&clean(), &[("c3", "needs its own table")]);
        assert!(r.findings[1].contains("listed in UNLINTED (needs its own table); remove"));
        let r = check(&clean(), &[("gone", "x")]);
        assert!(r.findings[1].contains("crates/gone: listed in UNLINTED but not a crate"));
    }

    #[test]
    fn a_crate_that_is_too_few_fails() {
        let mut m = clean();
        m.files.retain(|p, _| !p.starts_with("crates/c1"));
        assert!(check(&m, &[]).findings[1].contains("the scan is broken"));
    }
}
