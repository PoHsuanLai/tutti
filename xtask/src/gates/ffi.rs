//! `check-ffi`: `unsafe` lives only in the crates that exist to hold it.
//!
//! Replaces `check-plugin-ffi.sh` and `check-midi-ffi.sh` (design 016 section
//! 2.9, design 017 stage 4d). The compiler does the work those scripts
//! repeated: a crate that carries `#![forbid(unsafe_code)]` cannot hold an
//! `unsafe` token (not in a test inside `src/` either), so what is left to
//! check is that the attribute is there.
//!
//! - **Every crate root forbids `unsafe`**, `src/lib.rs` and `src/main.rs`
//!   alike, except the crates in [`HOMES`]: the `-ffi` crates, `tutti-rt`,
//!   `arc-lend`, `tutti-plugin-types` (which denies it and allows one
//!   constructor) and the parked `bevy-tutti`. This subsumes the old host
//!   crates' scans, `plugin-server`'s loaders and the VST2 caller pins. A home
//!   that forbids `unsafe` after all, or is gone, is a stale entry.
//! - **The `-ffi` crates carry the lints** that make every `unsafe` block
//!   a `// SAFETY:` comment and every `unsafe fn` a `# Safety` section:
//!   `undocumented_unsafe_blocks` and `missing_safety_doc`, in a
//!   `#![deny(..)]` of the crate root. Clippy cannot check its own lints are
//!   on.
//! - **`tutti-midi-hardware`** has no `unsafe` outside `src/` either (`build.rs`,
//!   `tests/`, `examples/`, `benches/` are separate crates the forbid does not
//!   cover), and names none of the raw bindings in its manifest.
//!
//! Comments and string literals are not code.

use super::lex::{blank, find_word, line_of};
use super::{Report, Tree};

const SKIPPED_DIRS: &[&str] = &["target", ".git"];

fn skipped(dir: &str) -> bool {
    SKIPPED_DIRS.contains(&dir)
}

/// `(crate directory, why it may hold unsafe, the lint it must carry instead)`.
pub(super) const HOMES: &[(&str, &str, Option<&str>)] = &[
    ("arc-lend", "the publish cell", None),
    (
        "tutti-rt",
        "the lock-free and unsafe runtime structure",
        None,
    ),
    ("tutti-alsa-ffi", "all ALSA unsafe", None),
    (
        "tutti-coremidi-ffi",
        "all CoreMIDI and Mach clock unsafe",
        None,
    ),
    ("tutti-clap-ffi", "all CLAP unsafe", None),
    ("tutti-vst3-ffi", "all VST3 unsafe", None),
    ("tutti-au-ffi", "all AU unsafe", None),
    ("tutti-vst2-ffi", "all VST2 unsafe", None),
    ("tutti-plugin-ipc-ffi", "the process edge's unsafe", None),
    (
        "tutti-plugin-types",
        "one documented `unsafe` constructor (`WindowHandle::from_u64`)",
        Some("deny(unsafe_code)"),
    ),
    (
        "bevy-tutti",
        "parked: out of the workspace, its crate root comes back with un-parking",
        None,
    ),
];

/// The crates whose `unsafe` must be documented.
const FFI: &[&str] = &[
    "tutti-clap-ffi",
    "tutti-vst3-ffi",
    "tutti-au-ffi",
    "tutti-vst2-ffi",
    "tutti-alsa-ffi",
    "tutti-coremidi-ffi",
];
const LINTS: &[&str] = &["undocumented_unsafe_blocks", "missing_safety_doc"];

const MIDI: &str = "crates/tutti-midi-hardware";
/// Dependencies that exist only to make unsafe calls: the `-ffi` crates' own.
const RAW_DEPS: &[&str] = &["libc", "coremidi-sys", "mach2", "tutti-alsa-ump-sys"];

pub(super) fn check(tree: &dyn Tree) -> Report {
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

    let mut exempt = 0;
    for name in &crates {
        let home = HOMES.iter().find(|(h, ..)| h == name);
        for root in ["lib.rs", "main.rs"] {
            let path = format!("crates/{name}/src/{root}");
            let Some(text) = tree.read(&path) else {
                continue;
            };
            let code = blank(&text);
            let forbids = code.contains("#![forbid(unsafe_code)]");
            match home {
                None if !forbids => bad.push(format!(
                    "{path}: missing #![forbid(unsafe_code)] (a crate that holds no unsafe forbids it; \
                     one that must goes in HOMES in xtask/src/gates/ffi.rs)"
                )),
                Some((_, why, _)) if forbids => bad.push(format!(
                    "{path}: forbids unsafe_code but is listed in HOMES ({why}); remove the entry"
                )),
                Some((_, why, Some(attr))) if !code.contains(&format!("#![{attr}]")) => {
                    bad.push(format!("{path}: missing #![{attr}] ({why})"));
                }
                _ => {}
            }
        }
        exempt += usize::from(home.is_some());
    }
    for (name, ..) in HOMES {
        if !crates.contains(name) {
            bad.push(format!(
                "crates/{name}: listed in HOMES but not a crate; was it moved?"
            ));
        }
    }

    for name in FFI {
        let path = format!("crates/{name}/src/lib.rs");
        let Some(text) = tree.read(&path) else {
            bad.push(format!("{path}: missing; was the crate moved?"));
            continue;
        };
        let code = blank(&text);
        for lint in LINTS {
            if !denies(&code, lint) {
                bad.push(format!("{path}: missing #![deny(clippy::{lint})]"));
            }
        }
    }

    // The midi backend: the targets the crate-level forbid does not reach.
    let midi: Vec<String> = tree
        .files(MIDI, &skipped)
        .into_iter()
        .filter(|p| {
            let rel = &p[MIDI.len() + 1..];
            rel == "build.rs"
                || ["tests/", "examples/", "benches/"]
                    .iter()
                    .any(|d| rel.starts_with(d))
        })
        .filter(|p| p.ends_with(".rs"))
        .collect();
    for path in &midi {
        let Some(text) = tree.read(path) else {
            continue;
        };
        let code = blank(&text);
        if let Some(at) = find_word(&code, "unsafe") {
            bad.push(format!(
                "{path}:{}: `unsafe` in {MIDI} (its targets hold none)",
                line_of(&code, at)
            ));
        }
    }
    match tree.read(&format!("{MIDI}/Cargo.toml")) {
        None => bad.push(format!("{MIDI}/Cargo.toml: missing; was the crate moved?")),
        Some(manifest) => {
            for dep in RAW_DEPS {
                let named = manifest.lines().any(|l| {
                    l.trim_start()
                        .strip_prefix(dep)
                        .is_some_and(|r| r.trim_start().starts_with(['=', '.']))
                });
                if named {
                    bad.push(format!(
                        "{MIDI}/Cargo.toml: depends on `{dep}` (raw FFI belongs to the -ffi crates)"
                    ));
                }
            }
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        r.say(format!(
            "check-ffi: ok ({} crates, {exempt} hold unsafe by name, {} midi targets)",
            crates.len(),
            midi.len()
        ));
    } else {
        r.fail("check-ffi: FAIL");
        for b in bad {
            r.fail(format!("  {b}"));
        }
    }
    r
}

/// Whether a `#![deny(..)]` in the blanked crate root names `lint`
/// (`clippy::lint`), the regex `#!\[deny\([^\]]*lint`.
fn denies(code: &str, lint: &str) -> bool {
    code.match_indices("#![deny(").any(|(i, m)| {
        let rest = &code[i + m.len()..];
        rest.split(']')
            .next()
            .is_some_and(|attr| attr.contains(lint))
    })
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const FORBID: &str = "#![forbid(unsafe_code)]\n";
    const DENY_LINTS: &str =
        "#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]\n";

    /// A tree that passes: 20 forbidding crates, the homes, the midi crate.
    fn clean() -> Mem {
        let mut m = Mem::default();
        let mut put = |p: String, t: &str| {
            m.files.insert(p, t.to_owned());
        };
        for i in 0..20 {
            put(format!("crates/c{i}/Cargo.toml"), "");
            put(format!("crates/c{i}/src/lib.rs"), FORBID);
        }
        for (name, _, attr) in HOMES {
            put(format!("crates/{name}/Cargo.toml"), "");
            let lib = match attr {
                Some(a) => format!("#![{a}]\n"),
                None if FFI.contains(name) => DENY_LINTS.to_owned(),
                None => "unsafe fn f() {}\n".to_owned(),
            };
            put(format!("crates/{name}/src/lib.rs"), &lib);
        }
        put(
            format!("{MIDI}/Cargo.toml"),
            "[dependencies]\nthiserror = \"2\"\n",
        );
        put(format!("{MIDI}/src/lib.rs"), FORBID);
        put(format!("{MIDI}/tests/t.rs"), "fn t() {}\n");
        m.files.remove("crates/bevy-tutti/src/lib.rs");
        m
    }

    fn with(mut m: Mem, path: &str, text: &str) -> Report {
        m.files.insert(path.to_owned(), text.to_owned());
        check(&m)
    }

    #[test]
    fn a_clean_tree_passes() {
        let r = check(&clean());
        assert!(r.passed(), "{:?}", r.findings);
        assert!(
            r.out[0].starts_with("check-ffi: ok (32 crates"),
            "{:?}",
            r.out
        );
    }

    #[test]
    fn a_crate_root_without_the_forbid_fails() {
        let r = with(clean(), "crates/c3/src/lib.rs", "pub fn f() {}\n");
        assert_eq!(r.findings[0], "check-ffi: FAIL");
        assert!(
            r.findings[1].starts_with("  crates/c3/src/lib.rs: missing #![forbid(unsafe_code)]")
        );
    }

    #[test]
    fn a_forbid_in_a_comment_is_not_one_and_a_main_rs_is_a_root() {
        let r = with(
            clean(),
            "crates/c3/src/lib.rs",
            "// #![forbid(unsafe_code)]\n",
        );
        assert_eq!(r.findings.len(), 2);
        let r = with(clean(), "crates/c4/src/main.rs", "fn main() {}\n");
        assert!(r.findings[1].contains("crates/c4/src/main.rs: missing"));
    }

    #[test]
    fn a_home_that_forbids_is_stale_and_a_missing_home_is_reported() {
        let r = with(clean(), "crates/tutti-rt/src/lib.rs", FORBID);
        assert!(r.findings[1].contains("listed in HOMES"));
        let mut m = clean();
        m.files.remove("crates/tutti-rt/Cargo.toml");
        assert!(check(&m).findings[1].contains("crates/tutti-rt: listed in HOMES but not a crate"));
    }

    #[test]
    fn the_denying_home_must_deny() {
        let r = with(
            clean(),
            "crates/tutti-plugin-types/src/lib.rs",
            "pub fn f() {}\n",
        );
        assert!(r.findings[1].contains("missing #![deny(unsafe_code)]"));
    }

    #[test]
    fn an_ffi_crate_without_its_lints_fails() {
        let r = with(
            clean(),
            "crates/tutti-clap-ffi/src/lib.rs",
            "#![deny(clippy::undocumented_unsafe_blocks)]\n",
        );
        assert_eq!(
            r.findings[1],
            "  crates/tutti-clap-ffi/src/lib.rs: missing #![deny(clippy::missing_safety_doc)]"
        );
        let r = with(
            clean(),
            "crates/tutti-alsa-ffi/src/lib.rs",
            "// #![deny(clippy::missing_safety_doc, clippy::undocumented_unsafe_blocks)]\n",
        );
        assert_eq!(r.findings.len(), 3);
    }

    #[test]
    fn midi_targets_and_manifest_are_held() {
        let r = with(
            clean(),
            "crates/tutti-midi-hardware/tests/t.rs",
            "fn t() { unsafe {} }\n",
        );
        assert_eq!(
            r.findings[1],
            "  crates/tutti-midi-hardware/tests/t.rs:1: `unsafe` in crates/tutti-midi-hardware (its targets hold none)"
        );
        let r = with(
            clean(),
            "crates/tutti-midi-hardware/tests/t.rs",
            "// unsafe\nconst S: &str = \"unsafe\";\n",
        );
        assert!(r.passed());
        let r = with(
            clean(),
            "crates/tutti-midi-hardware/Cargo.toml",
            "[dependencies]\nlibc = \"0.2\"\nmach2.workspace = true\n# coremidi-sys\n",
        );
        assert_eq!(r.findings.len(), 3);
        assert!(r.findings[1].contains("`libc`") && r.findings[2].contains("`mach2`"));
    }

    #[test]
    fn a_scan_of_nothing_fails() {
        let r = check(&Mem::default());
        assert!(r.findings[1].starts_with("  only 0 crates found"));
    }
}
