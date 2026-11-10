//! `check-submodule-pins`: the VST3 SDK is three Steinberg submodules, and
//! this asserts each one is at the commit the repo documents as the
//! `v3.8.0_build_66` release.
//!
//! Three places state that pin in prose (`.gitmodules`, `CLAUDE.md`, and the
//! `VENDORED_SDK` doc comment in `crates/tutti-vst3-host/build.rs`) and prose
//! cannot notice when a submodule bump moves the tree out from under it. This
//! is the only check that can.
//!
//! It asserts SHAs rather than `git describe` output, and that choice is the
//! whole reason the gate is not a one-line grep for the tag name. In a normal
//! clone `git describe` reports `v3.7.3_build_20-10-g3d2e82f` and two like it,
//! naming a release these trees are well past: a submodule fetch brings the
//! commit its superproject records, not every tag that happens to point near
//! it, so the 3.8.0 tag was never fetched here and describe falls back to the
//! nearest ANCESTOR tag it does have. The checked-out content really is 3.8.0:
//! `LICENSE.txt` in all three is the MIT relicensing that shipped with that
//! release ("Copyright (c) 2025, Steinberg Media Technologies GmbH"), and
//! `docs/reference/vst3-3.8.0-interface-surface.md` corroborates it against
//! the headers. A gate built on describe would therefore fail loudly on a
//! perfectly correct tree, and a gate that cries wolf is a gate somebody
//! deletes. A SHA cannot be wrong about which commit is checked out, and it is
//! what the superproject actually records, so that is what this compares.
//!
//! Mutation record (each planted, seen to fail the named test, removed):
//! - drop the `'-'` arm: `an_uninitialised_submodule_is_not_a_mismatch`;
//! - drop the `'+'` arm: `a_submodule_at_another_commit_than_recorded_is_named`;
//! - drop the `'U'` arm: `a_merge_conflict_is_named`;
//! - `_ if have != want` made `_ if false`: `a_mismatch_prints_both_shas_and_the_summary`;
//! - the "git knows no submodule" lines reported as output, not findings:
//!   `a_path_git_does_not_know_fails`;
//! - drop the final summary (`if !r.passed()` made `if false`): every failing
//!   test above that asserts it.

use super::{Git, Report};

/// The `v3.8.0_build_66` commits, one `(path, sha)` pair per submodule.
/// Bumping the SDK means editing these three lines in the same commit as the
/// submodule move; that is the point of the gate, not an inconvenience it
/// imposes.
pub(super) const PINS: &[(&str, &str)] = &[
    (
        "vendor/vst3-sdk/base",
        "3d2e82f8e6bff59c1d8b7a27491a29c2286b5206",
    ),
    (
        "vendor/vst3-sdk/pluginterfaces",
        "31d6eeba6daaa3e2a8bfbe3e7a90ca0b7fbfbc1c",
    ),
    (
        "vendor/vst3-sdk/public.sdk",
        "a3911a4615dabbfdfd9d181ee26b05c70c289a95",
    ),
];

pub(super) fn check(git: &dyn Git, pins: &[(&str, &str)]) -> Report {
    let mut r = Report::default();
    r.say("==> VST3 SDK submodule pins (v3.8.0_build_66)");

    for &(path, want) in pins {
        let Some(line) = git.submodule_status(path) else {
            r.fail(format!("FAIL {path}"));
            r.fail("     git knows no submodule at this path. It has been renamed or");
            r.fail("     dropped from .gitmodules; fix the path here and in the three");
            r.fail("     places that document the pin in prose.");
            continue;
        };

        // `git submodule status` prefixes the SHA with exactly one status
        // character: a space when the working tree sits at the recorded
        // commit, `-` when the submodule is not initialised, `+` when it is
        // checked out at some other commit, and `U` during a merge conflict.
        // Stripping it is not cosmetic: leaving it on compares a 41-character
        // string against a 40-character one, so every entry would report a
        // mismatch, and for a reason the output would not explain.
        let mut chars = line.chars();
        let flag = chars.next();
        let have = chars.as_str().split([' ', '\n']).next().unwrap_or_default();

        match flag {
            // Distinguished from a mismatch deliberately: an uninitialised
            // submodule still prints the SHA the superproject records, so a
            // naive comparison would PASS on an empty directory and report a
            // healthy SDK that is not on disk at all. Say what is actually
            // wrong and how to fix it.
            Some('-') => {
                r.fail(format!("FAIL {path}"));
                r.fail("     submodule not initialised \u{2014} the directory is empty, so there");
                r.fail("     is no SDK here to check and tutti-vst3-host cannot build its");
                r.fail("     audio-probe reference plugin.");
                r.fail("     Run: git submodule update --init --recursive");
            }
            Some('U') => {
                r.fail(format!("FAIL {path}"));
                r.fail("     submodule has a merge conflict; resolve it before the pin can");
                r.fail("     be checked.");
            }
            // The SHA printed after `+` is the one checked out, not the one
            // recorded in the index, so the two disagree whatever the
            // comparison below would say. Committing from here would move the
            // pin as a side effect of whatever left the submodule detached.
            Some('+') => {
                r.fail(format!("FAIL {path}"));
                r.fail(format!(
                    "     working tree is at {have}, which is not the commit the"
                ));
                r.fail("     superproject records. Run: git submodule update --recursive");
            }
            _ if have != want => {
                r.fail(format!("FAIL {path}"));
                r.fail(format!("     expected {want}"));
                r.fail(format!("     found    {have}"));
                r.fail("     If the SDK was bumped on purpose, update `PINS` in xtask/src/gates/submodule_pins.rs and the");
                r.fail("     three prose mentions (.gitmodules, CLAUDE.md, build.rs) too.");
            }
            _ => r.say(format!("ok   {path}")),
        }
    }

    if !r.passed() {
        r.fail("FAIL: the vendored VST3 SDK is not at the documented v3.8.0_build_66 pin.");
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    fn git(status: &[(&str, &str)]) -> Mem {
        let mut m = Mem::default();
        for (p, s) in status {
            m.status.insert((*p).to_owned(), (*s).to_owned());
        }
        m
    }

    const PIN: &[(&str, &str)] = &[("vendor/x", A)];

    fn one(status: &str) -> Report {
        check(&git(&[("vendor/x", status)]), PIN)
    }

    #[test]
    fn a_pinned_tree_passes() {
        let r = one(&format!(" {A} vendor/x (v3.7.3_build_20-10-g1111111)"));
        assert!(r.passed());
        assert_eq!(
            r.out,
            [
                "==> VST3 SDK submodule pins (v3.8.0_build_66)",
                "ok   vendor/x"
            ]
        );
    }

    #[test]
    fn the_real_pins_are_three_forty_digit_shas() {
        assert_eq!(PINS.len(), 3);
        for (path, sha) in PINS {
            assert!(path.starts_with("vendor/vst3-sdk/"), "{path}");
            assert!(sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    // Mutation: drop the `None` arm's body (treat as ok): this fails.
    #[test]
    fn a_path_git_does_not_know_fails() {
        let r = check(&Mem::default(), PIN);
        assert_eq!(
            r.findings,
            [
                "FAIL vendor/x",
                "     git knows no submodule at this path. It has been renamed or",
                "     dropped from .gitmodules; fix the path here and in the three",
                "     places that document the pin in prose.",
                "FAIL: the vendored VST3 SDK is not at the documented v3.8.0_build_66 pin.",
            ]
        );
    }

    #[test]
    fn an_uninitialised_submodule_is_not_a_mismatch() {
        // It prints the recorded SHA, which equals the pin: a naive comparison
        // would pass.
        let r = one(&format!("-{A} vendor/x"));
        assert_eq!(
            r.findings[..5],
            [
                "FAIL vendor/x",
                "     submodule not initialised \u{2014} the directory is empty, so there",
                "     is no SDK here to check and tutti-vst3-host cannot build its",
                "     audio-probe reference plugin.",
                "     Run: git submodule update --init --recursive",
            ]
        );
        assert_eq!(r.findings.len(), 6);
    }

    #[test]
    fn a_merge_conflict_is_named() {
        let r = one(&format!("U{A} vendor/x"));
        assert_eq!(
            r.findings[..3],
            [
                "FAIL vendor/x",
                "     submodule has a merge conflict; resolve it before the pin can",
                "     be checked.",
            ]
        );
        assert_eq!(r.findings.len(), 4);
    }

    #[test]
    fn a_submodule_at_another_commit_than_recorded_is_named() {
        // Even when the checked-out commit is the pin: `+` means it is not
        // what the superproject records.
        let r = one(&format!("+{A} vendor/x (heads/main)"));
        assert_eq!(
            r.findings[..3],
            [
                "FAIL vendor/x".to_owned(),
                format!("     working tree is at {A}, which is not the commit the"),
                "     superproject records. Run: git submodule update --recursive".to_owned(),
            ]
        );
        assert_eq!(r.findings.len(), 4);
    }

    #[test]
    fn a_mismatch_prints_both_shas_and_the_summary() {
        let r = one(&format!(" {B} vendor/x (v3.7.3_build_20)"));
        assert_eq!(
            r.findings,
            [
                "FAIL vendor/x".to_owned(),
                format!("     expected {A}"),
                format!("     found    {B}"),
                "     If the SDK was bumped on purpose, update `PINS` in xtask/src/gates/submodule_pins.rs and the".to_owned(),
                "     three prose mentions (.gitmodules, CLAUDE.md, build.rs) too.".to_owned(),
                "FAIL: the vendored VST3 SDK is not at the documented v3.8.0_build_66 pin."
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn every_pin_is_checked_and_a_good_one_does_not_hide_a_bad_one() {
        let pins = [("vendor/a", A), ("vendor/b", A), ("vendor/c", B)];
        let g = git(&[
            ("vendor/a", &format!(" {A} vendor/a")),
            ("vendor/b", &format!(" {B} vendor/b")),
            ("vendor/c", &format!(" {B} vendor/c")),
        ]);
        let r = check(&g, &pins);
        assert_eq!(r.out[1], "ok   vendor/a");
        assert_eq!(r.out[2], "FAIL vendor/b");
        assert_eq!(r.out[7], "ok   vendor/c");
        assert_eq!(r.findings.len(), 6);
    }
}
