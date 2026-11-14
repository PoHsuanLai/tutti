//! `check-platform-seam`: a platform difference sits behind one seam; the `cfg`
//! chooses a platform module, never each item.
//!
//! In `crates/*/src`, `crates/*/tests`, `testing/*/src` and `testing/*/tests`, a
//! *platform cfg* may stand only on a `mod` declaration: `#[cfg(unix)] mod x;`,
//! `#[cfg(unix)] mod x { .. }` or `#[cfg_attr(unix, path = "x.rs")] mod x;`
//! (further attributes between it and the `mod` are fine). Anywhere else (a
//! `fn`, `use`, `impl`, field, statement, a `cfg!(..)` expression, a
//! `cfg_attr` that adds a non-`path` attribute) it is a violation.
//!
//! A platform cfg is a `cfg(..)` or `cfg_attr(..)` predicate (for `cfg_attr`,
//! its first argument, or a `cfg(..)` among the attributes it adds) or a
//! `cfg!(..)` whose text, at any `all`/`any`/`not` depth, names `unix`,
//! `windows` or a `target_*` key (`os`, `family`, `vendor`, `env`, `arch`,
//! `pointer_width`, `endian`, `has_atomic`, `feature`). A feature, `test`,
//! `loom`, `debug_assertions`, `miri` or `doc` cfg is not one, but
//! `all(test, unix)` is. Comments and literals are blanked first, so prose that
//! says `cfg(unix)` is not a cfg, and a multi-line attribute is read whole.
//!
//! A file under `tests/` may open with the inner attribute `#![cfg(<platform>)]`:
//! a test binary's root cannot be gated from a parent, so the whole file is the
//! platform unit. In `src/` an inner platform cfg is a violation like any other.
//!
//! Exempt: the `tutti-*-ffi` crates, where the platform code lives (CLAUDE.md:
//! plugin, process-edge and MIDI OS `unsafe` sits in them). Out of scope: a
//! `[target.'cfg(..)'.dependencies]` table in a manifest, `build.rs`,
//! `examples/` and `benches/`.
//!
//! The ratchet is [`ALLOW`]: one row per file that still holds violations,
//! `(file, how many, why)`. A file over its row fails (a new violation), a file
//! under it fails (lower the row), and a row whose file holds none, or is gone,
//! is stale. Rows are keyed by count, not line, so moving code in a file does
//! not edit them. A reason is `temporary: <session> moves it behind a module
//! seam` with the session that owns the crate (see [`owner`]), or
//! `permanent: <argument>` for what cannot move.

use std::collections::BTreeMap;

use super::lex::{blank, find_word_from, line_of, matching};
use super::{Report, Tree};

const CORE: &str = "temporary: core moves it behind a module seam";
const DSP: &str = "temporary: dsp moves it behind a module seam";
const MIDI: &str = "temporary: midi moves it behind a module seam";
const PLUGINS: &str = "temporary: plugins moves it behind a module seam";

/// `(file, platform cfgs outside a mod declaration, reason)`.
pub(super) const ALLOW: &[(&str, usize, &str)] = &[
    ("crates/tutti-rt/src/clock.rs", 7, CORE),
    ("crates/tutti-rt/src/denormals.rs", 7, CORE),
    ("crates/tutti-rt/src/denormals/tests.rs", 14, CORE),
    ("crates/tutti-rt/src/job_gate/tests.rs", 2, CORE),
    ("crates/tutti-rt/src/priority.rs", 8, CORE),
    ("crates/tutti-rt/src/spin.rs", 13, CORE),
    ("crates/tutti-rt/src/spin/tests.rs", 1, CORE),
    ("crates/tutti-rt/src/task_graph/tests.rs", 2, CORE),
    (
        "crates/tutti-session/tests/export/export_fork_sampler.rs",
        2,
        CORE,
    ),
    (
        "crates/tutti-session/tests/plugin/export_fork_plugin.rs",
        2,
        CORE,
    ),
    ("crates/tutti-session/tests/plugin/plugin.rs", 5, CORE),
    (
        "crates/tutti-session/tests/plugin/plugin_diagnostics.rs",
        1,
        CORE,
    ),
    ("testing/tutti-fixtures/src/glibc.rs", 5, CORE),
    ("testing/tutti-fixtures/src/lib.rs", 9, CORE),
    ("testing/tutti-fixtures/src/plugin.rs", 2, CORE),
    ("testing/tutti-fixtures/src/vst3_cpp.rs", 2, CORE),
    ("crates/tutti-dsp/src/resample/fft/tests.rs", 1, DSP),
    ("crates/tutti-midi-hardware/src/backend/mod.rs", 3, MIDI),
    ("crates/tutti-midi-hardware/src/devices.rs", 4, MIDI),
    ("crates/tutti-midi-hardware/src/error.rs", 2, MIDI),
    ("crates/tutti-midi-hardware/src/lib.rs", 1, MIDI),
    ("crates/tutti-midi-hardware/src/stamp.rs", 2, MIDI),
    ("crates/tutti-midi-hardware/src/testing.rs", 1, MIDI),
    ("crates/tutti-au-host/src/unit_code.rs", 1, PLUGINS),
    (
        "crates/tutti-au-host/tests/au_gui_lifecycle_main.rs",
        4,
        PLUGINS,
    ),
    (
        "crates/tutti-clap-host/tests/behaviour/unit_tests.rs",
        3,
        PLUGINS,
    ),
    (
        "crates/tutti-clap-host/tests/conformance/clap_gui_lifecycle.rs",
        1,
        PLUGINS,
    ),
    ("crates/tutti-plugin-ipc/src/config/bridge.rs", 2, PLUGINS),
    ("crates/tutti-plugin-ipc/src/ipc/tests.rs", 4, PLUGINS),
    (
        "crates/tutti-plugin-ipc/src/subprocess/bundle.rs",
        6,
        PLUGINS,
    ),
    (
        "crates/tutti-plugin-ipc/src/subprocess/launch.rs",
        10,
        PLUGINS,
    ),
    ("crates/tutti-plugin-probe/src/lib.rs", 3, PLUGINS),
    ("crates/tutti-plugin-server/src/lib.rs", 3, PLUGINS),
    ("crates/tutti-plugin-server/src/served/any.rs", 1, PLUGINS),
    (
        "crates/tutti-plugin-server/src/served/format.rs",
        1,
        PLUGINS,
    ),
    ("crates/tutti-plugin-server/src/served/tests.rs", 1, PLUGINS),
    ("crates/tutti-plugin-server/src/transport.rs", 1, PLUGINS),
    (
        "crates/tutti-plugin-server/tests/orphan_watchdog.rs",
        2,
        PLUGINS,
    ),
    ("crates/tutti-plugin-types/src/bundle.rs", 13, PLUGINS),
    ("crates/tutti-plugin-types/src/editor.rs", 2, PLUGINS),
    (
        "crates/tutti-plugin-types/src/lifecycle/tests.rs",
        2,
        PLUGINS,
    ),
    ("crates/tutti-plugin-types/src/probe_wire.rs", 1, PLUGINS),
    ("crates/tutti-plugin/src/format/gui/mod.rs", 4, PLUGINS),
    ("crates/tutti-plugin/src/host/catalog.rs", 1, PLUGINS),
    (
        "crates/tutti-plugin/src/host/node/real_stall_tests.rs",
        1,
        PLUGINS,
    ),
    ("crates/tutti-plugin/src/host/plugin.rs", 10, PLUGINS),
    ("crates/tutti-plugin/src/host/plugin_ref.rs", 3, PLUGINS),
    (
        "crates/tutti-plugin/tests/clap_nodes/clap_fork.rs",
        2,
        PLUGINS,
    ),
    (
        "crates/tutti-plugin/tests/clap_nodes/clap_live.rs",
        2,
        PLUGINS,
    ),
    (
        "crates/tutti-plugin/tests/real_plugin_pressure.rs",
        9,
        PLUGINS,
    ),
    (
        "crates/tutti-plugin/tests/support/clap_probe.rs",
        2,
        PLUGINS,
    ),
    (
        "crates/tutti-plugin/tests/vst2_nodes/vst2_in_process_fork.rs",
        1,
        PLUGINS,
    ),
    (
        "crates/tutti-vst3-host/tests/conformance/vst3_com_dispatch.rs",
        6,
        PLUGINS,
    ),
    (
        "crates/tutti-vst3-host/tests/conformance/vst3_conformance_interfaces.rs",
        4,
        PLUGINS,
    ),
    (
        "crates/tutti-vst3-host/tests/conformance/vst3_editor.rs",
        1,
        PLUGINS,
    ),
    (
        "crates/tutti-vst3-host/tests/host/integration_tests.rs",
        1,
        PLUGINS,
    ),
    (
        "crates/tutti-vst3-host/tests/host/vst3_ffi_plugin.rs",
        3,
        PLUGINS,
    ),
    (
        "crates/tutti-vst3-host/tests/conformance/vst3_traits.rs",
        1,
        PLUGINS,
    ),
    ("testing/tutti-clap-test-plugin/src/gui.rs", 3, PLUGINS),
    (
        "testing/tutti-clap-test-plugin/src/subprocess.rs",
        1,
        PLUGINS,
    ),
    ("testing/tutti-plugin-contract/src/formats.rs", 2, PLUGINS),
    ("testing/tutti-plugin-contract/src/lib.rs", 2, PLUGINS),
    ("testing/tutti-plugin-contract/src/plugin.rs", 1, PLUGINS),
    ("testing/tutti-shared-plugin/src/lib.rs", 1, PLUGINS),
    ("testing/tutti-shared-plugin/src/vst3.rs", 18, PLUGINS),
    ("testing/tutti-vst2-author/src/lib.rs", 4, PLUGINS),
    ("testing/tutti-vst2-test-plugin/src/lib.rs", 2, PLUGINS),
];

const PLATFORM: &[&str] = &[
    "unix",
    "windows",
    "target_os",
    "target_family",
    "target_vendor",
    "target_env",
    "target_arch",
    "target_pointer_width",
    "target_endian",
    "target_has_atomic",
    "target_feature",
];

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git")
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Whether `text` names a platform key as a whole word.
fn mentions_platform(text: &str) -> bool {
    text.split(|c: char| !word(c as u8) || !c.is_ascii())
        .any(|w| PLATFORM.contains(&w))
}

/// The session that owns a crate, by its directory under `crates/` or `testing/`.
pub(super) fn owner(krate: &str) -> Option<&'static str> {
    const CORE_CRATES: &[&str] = &[
        "tutti-types",
        "tutti-rt",
        "tutti-graph",
        "tutti-engine",
        "tutti-session",
        "tutti-transport",
        "tutti-tempo",
        "tutti-curve",
        "tutti-modulation",
        "tutti-codec",
        "tutti-export",
        "tutti-sampler",
        "tutti-cpal",
        "tutti",
        "tutti-fixtures",
    ];
    const DSP_CRATES: &[&str] = &[
        "tutti-dsp",
        "tutti-nodes",
        "tutti-measure",
        "tutti-polysynth",
        "tutti-soundfont",
        "tutti-sf2",
        "tutti-metering",
    ];
    const MIDI_CRATES: &[&str] = &["tutti-ump", "tutti-voicing"];
    const PLUGIN_CRATES: &[&str] = &[
        "tutti-clap-test-plugin",
        "tutti-shared-plugin",
        "tutti-shared-plugin-vst3-cpp",
        "tutti-vst2-author",
        "tutti-vst2-test-plugin",
    ];
    if CORE_CRATES.contains(&krate) {
        Some("core")
    } else if DSP_CRATES.contains(&krate) {
        Some("dsp")
    } else if MIDI_CRATES.contains(&krate) || krate.starts_with("tutti-midi-") {
        Some("midi")
    } else if PLUGIN_CRATES.contains(&krate)
        || krate.starts_with("tutti-plugin")
        || krate.ends_with("-host")
    {
        Some("plugins")
    } else {
        None
    }
}

/// The crate directory of a scanned file and whether it is under `tests/`.
fn scanned(path: &str) -> Option<(&str, bool)> {
    let mut parts = path.split('/');
    let (root, krate, dir) = (parts.next()?, parts.next()?, parts.next()?);
    let in_tests = dir == "tests";
    (matches!(root, "crates" | "testing")
        && (dir == "src" || in_tests)
        && path.ends_with(".rs")
        && parts.next().is_some())
    .then_some((krate, in_tests))
}

fn ffi_crate(krate: &str) -> bool {
    krate.starts_with("tutti-") && krate.ends_with("-ffi")
}

/// The offsets and kinds of the platform cfgs in blanked `code` that stand
/// outside a mod declaration.
fn violations(code: &str, in_tests: bool) -> Vec<usize> {
    let b = code.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let ws = |mut i: usize| {
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut i = 0;
    while i < n {
        if b[i] == b'#' {
            let mut j = ws(i + 1);
            let inner = b.get(j) == Some(&b'!');
            if inner {
                j = ws(j + 1);
            }
            if b.get(j) == Some(&b'[')
                && let Some(close) = matching(code, j, b'[', b']')
            {
                let body = &code[j + 1..close];
                if platform_attr(body) {
                    let ok = if inner {
                        in_tests && code[..i].trim().is_empty()
                    } else {
                        on_mod(code, close + 1)
                    };
                    if !ok {
                        out.push(i);
                    }
                }
                i = close + 1;
                continue;
            }
        } else if code[i..].starts_with("cfg") && (i == 0 || !word(b[i - 1])) {
            let j = ws(i + 3);
            if b.get(j) == Some(&b'!')
                && let k = ws(j + 1)
                && b.get(k) == Some(&b'(')
                && let Some(close) = matching(code, k, b'(', b')')
            {
                if mentions_platform(&code[k + 1..close]) {
                    out.push(i);
                }
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Whether an attribute body (between `[` and `]`) is a platform `cfg` or `cfg_attr`.
fn platform_attr(body: &str) -> bool {
    let body = body.trim_start();
    let name_end = body.find(|c: char| !word(c as u8)).unwrap_or(body.len());
    let (name, rest) = body.split_at(name_end);
    let rest = rest.trim_start();
    let Some(args) = rest.strip_prefix('(') else {
        return false;
    };
    match name {
        "cfg" => mentions_platform(args),
        "cfg_attr" => {
            let mut depth = 0usize;
            let split = args.find(|c: char| {
                match c {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth = depth.saturating_sub(1),
                    ',' if depth == 0 => return true,
                    _ => {}
                }
                false
            });
            let (pred, added) = split.map_or((args, ""), |s| (&args[..s], &args[s..]));
            mentions_platform(pred)
                || (find_word_from(added, "cfg", 0).is_some() && mentions_platform(added))
        }
        _ => false,
    }
}

/// Whether the item that starts at `from` (after further attributes and a
/// visibility) is a `mod`.
fn on_mod(code: &str, from: usize) -> bool {
    let b = code.as_bytes();
    let n = b.len();
    let mut i = from;
    loop {
        while i < n && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b.get(i) == Some(&b'#') {
            let mut j = i + 1;
            while j < n && (b[j].is_ascii_whitespace() || b[j] == b'!') {
                j += 1;
            }
            match (b.get(j) == Some(&b'['))
                .then(|| matching(code, j, b'[', b']'))
                .flatten()
            {
                Some(close) => {
                    i = close + 1;
                    continue;
                }
                None => return false,
            }
        }
        break;
    }
    let mut rest = code[i..].trim_start();
    if let Some(r) = rest.strip_prefix("pub") {
        rest = r.trim_start();
        if rest.starts_with('(')
            && let Some(close) = matching(rest, 0, b'(', b')')
        {
            rest = rest[close + 1..].trim_start();
        }
    }
    rest.strip_prefix("mod")
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_whitespace()))
}

fn reason_problem(file: &str, reason: &str) -> Option<String> {
    let session = scanned(file).and_then(|(k, _)| owner(k));
    if let Some(argument) = reason.strip_prefix("permanent: ") {
        return (argument.trim().len() < 20)
            .then(|| "a `permanent:` row argues why the code cannot move".to_owned());
    }
    let Some(s) = reason
        .strip_prefix("temporary: ")
        .and_then(|r| r.strip_suffix(" moves it behind a module seam"))
    else {
        return Some(
            "reason is `temporary: <session> moves it behind a module seam` or `permanent: <why>`"
                .to_owned(),
        );
    };
    match session {
        Some(owner) if owner == s => None,
        Some(owner) => Some(format!(
            "the file belongs to the `{owner}` session, not `{s}`"
        )),
        None => Some("the crate has no owning session in `owner`".to_owned()),
    }
}

pub(super) fn check(tree: &dyn Tree, allow: &[(&str, usize, &str)]) -> Report {
    let mut bad = Vec::new();
    let mut found: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut seen = 0usize;
    for root in ["crates", "testing"] {
        for path in tree.files(root, &skipped) {
            let Some((krate, in_tests)) = scanned(&path) else {
                continue;
            };
            seen += 1;
            if ffi_crate(krate) {
                continue;
            }
            let Some(text) = tree.read(&path) else {
                continue;
            };
            let code = blank(&text);
            let lines: Vec<usize> = violations(&code, in_tests)
                .into_iter()
                .map(|at| line_of(&code, at))
                .collect();
            if !lines.is_empty() {
                found.insert(path, lines);
            }
        }
    }
    if seen == 0 && !allow.is_empty() {
        bad.push("no source file found; the scan is broken".to_owned());
    }

    let mut rows: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    for &(file, count, reason) in allow {
        if rows.insert(file, (count, reason)).is_some() {
            bad.push(format!("{file}: listed twice in ALLOW"));
        }
        if let Some(why) = reason_problem(file, reason) {
            bad.push(format!("{file}: ALLOW row: {why}"));
        }
    }
    for (file, lines) in &found {
        let n = lines.len();
        let at = |lines: &[usize]| {
            lines
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        match rows.get(file.as_str()) {
            None => bad.push(format!(
                "{file}: {n} platform cfg(s) outside a mod declaration (lines {}); \
                 put the platform code in a `sys/{{unix,windows}}`-style module chosen by one \
                 `#[cfg] mod`",
                at(lines)
            )),
            Some(&(count, _)) if n > count => bad.push(format!(
                "{file}: {n} platform cfgs, ALLOW holds {count} (lines {}); a new one is not allowed",
                at(lines)
            )),
            Some(&(count, _)) if n < count => bad.push(format!(
                "{file}: {n} platform cfgs left, ALLOW holds {count}; lower the row to {n}"
            )),
            Some(_) => {}
        }
    }
    for (file, (count, _)) in &rows {
        if !found.contains_key(*file) {
            bad.push(format!(
                "{file}: ALLOW holds {count} but the file has no violation left or is gone; remove the row"
            ));
        }
    }

    let mut r = Report::default();
    if bad.is_empty() {
        let held: usize = found.values().map(Vec::len).sum();
        r.say(format!(
            "check-platform-seam: OK ({held} allowed in {} files)",
            found.len()
        ));
    } else {
        r.fail("check-platform-seam: FAIL (a platform cfg outside a mod declaration):");
        for line in bad {
            r.fail(format!("  {line}"));
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    const GOOD: &str = "temporary: core moves it behind a module seam";

    fn run_with(files: &[(&str, &str)], allow: &[(&str, usize, &str)]) -> Vec<String> {
        check(&Mem::with(files), allow).findings
    }

    fn run(src: &str) -> Vec<String> {
        run_with(&[("crates/tutti-rt/src/x.rs", src)], &[])
    }

    #[test]
    fn a_cfg_on_a_mod_passes() {
        assert!(
            run("#[cfg(unix)]\nmod sys;\n#[cfg(windows)] pub(crate) mod w { fn f() {} }\n")
                .is_empty()
        );
        assert!(
            run("#[cfg(target_os = \"linux\")]\n#[path = \"a.rs\"]\n#[doc(hidden)]\nmod a;\n")
                .is_empty()
        );
        assert!(run("#[cfg(all(test, not(windows)))]\nmod t;\n").is_empty());
    }

    #[test]
    fn a_cfg_attr_path_on_a_mod_passes() {
        assert!(run("#[cfg_attr(unix, path = \"unix.rs\")]\n#[cfg_attr(windows, path = \"win.rs\")]\nmod sys;\n").is_empty());
    }

    #[test]
    fn a_cfg_on_a_fn_use_impl_or_statement_fails() {
        for src in [
            "#[cfg(unix)]\nfn f() {}\n",
            "#[cfg(unix)]\nuse a::b;\n",
            "#[cfg(windows)]\nimpl X {}\n",
            "fn f() {\n    #[cfg(target_arch = \"x86_64\")]\n    g();\n}\n",
            "struct S {\n    #[cfg(unix)]\n    x: u8,\n}\n",
            "#[cfg(unix)]\nconst A: u8 = 1;\n",
        ] {
            let got = run(src);
            assert_eq!(got.len(), 2, "{src}: {got:?}");
            assert!(
                got[1].contains("crates/tutti-rt/src/x.rs: 1 platform cfg(s)"),
                "{got:?}"
            );
        }
    }

    #[test]
    fn a_cfg_macro_in_an_expression_fails() {
        let got = run("fn f() -> bool {\n    cfg!(target_os = \"macos\")\n}\n");
        assert!(got[1].contains("(lines 2)"), "{got:?}");
        assert!(run("fn f() -> bool { cfg!(debug_assertions) }\n").is_empty());
    }

    #[test]
    fn a_cfg_attr_that_adds_something_other_than_a_path_fails() {
        assert_eq!(
            run("#[cfg_attr(unix, derive(Debug))]\nstruct S;\n").len(),
            2
        );
        assert_eq!(run("#[cfg_attr(unix, inline)]\nfn f() {}\n").len(), 2);
        assert_eq!(run("#[cfg_attr(test, cfg(unix))]\nfn f() {}\n").len(), 2);
    }

    #[test]
    fn a_feature_test_loom_or_miri_cfg_passes() {
        assert!(run("#[cfg(feature = \"unix\")]\nfn f() {}\n#[cfg(test)]\nfn g() {}\n#[cfg(loom)]\nuse a;\n#[cfg(not(miri))]\nfn h() {}\n#[cfg(any(debug_assertions, doc))]\nfn i() {}\n").is_empty());
    }

    #[test]
    fn a_predicate_mixing_a_platform_term_is_platform() {
        assert_eq!(run("#[cfg(all(test, unix))]\nfn f() {}\n").len(), 2);
        assert_eq!(run("#[cfg(any(loom, not(windows)))]\nfn f() {}\n").len(), 2);
        assert!(run("#[cfg(all(test, unix))]\nmod m;\n").is_empty());
    }

    #[test]
    fn a_multi_line_attribute_is_read_whole() {
        assert_eq!(
            run("#[cfg(any(\n    feature = \"a\",\n    target_os = \"macos\",\n))]\nfn f() {}\n")
                .len(),
            2
        );
        assert!(run("#[cfg(any(\n    feature = \"a\",\n    unix,\n))]\nmod m;\n").is_empty());
    }

    #[test]
    fn a_cfg_in_a_comment_or_string_is_not_a_cfg() {
        assert!(run("// #[cfg(unix)]\n/// `cfg!(windows)`\nfn f() { let s = \"#[cfg(unix)] fn\"; /* cfg!(unix) */ }\n").is_empty());
    }

    #[test]
    fn an_inner_platform_cfg_is_allowed_only_at_the_top_of_a_test_file() {
        let src = "#![cfg(target_os = \"macos\")]\nfn f() {}\n";
        assert!(run_with(&[("crates/tutti-rt/tests/a.rs", src)], &[]).is_empty());
        assert_eq!(run_with(&[("crates/tutti-rt/src/a.rs", src)], &[]).len(), 2);
    }

    #[test]
    fn an_ffi_crate_and_files_out_of_scope_are_not_read() {
        let bad = "#[cfg(unix)]\nfn f() {}\n";
        assert!(run_with(&[("crates/tutti-alsa-ffi/src/lib.rs", bad)], &[]).is_empty());
        assert!(
            run_with(
                &[
                    ("crates/tutti-rt/build.rs", bad),
                    ("crates/tutti-rt/examples/e.rs", bad)
                ],
                &[]
            )
            .is_empty()
        );
        assert_eq!(
            run_with(&[("testing/tutti-fixtures/tests/t.rs", bad)], &[]).len(),
            2
        );
    }

    #[test]
    fn a_row_holds_exactly_its_count() {
        let src = "#[cfg(unix)]\nfn f() {}\n#[cfg(unix)]\nfn g() {}\n";
        let file = [("crates/tutti-rt/src/x.rs", src)];
        assert!(run_with(&file, &[("crates/tutti-rt/src/x.rs", 2, GOOD)]).is_empty());
        let over = run_with(&file, &[("crates/tutti-rt/src/x.rs", 1, GOOD)]);
        assert!(
            over[1].contains("2 platform cfgs, ALLOW holds 1"),
            "{over:?}"
        );
        let under = run_with(&file, &[("crates/tutti-rt/src/x.rs", 3, GOOD)]);
        assert!(under[1].contains("lower the row to 2"), "{under:?}");
    }

    #[test]
    fn a_row_for_a_clean_or_missing_file_is_stale() {
        let clean = [("crates/tutti-rt/src/x.rs", "fn f() {}\n")];
        let got = run_with(&clean, &[("crates/tutti-rt/src/x.rs", 1, GOOD)]);
        assert!(got[1].contains("remove the row"), "{got:?}");
        let got = run_with(&clean, &[("crates/tutti-rt/src/gone.rs", 1, GOOD)]);
        assert!(got[1].contains("remove the row"), "{got:?}");
    }

    #[test]
    fn a_row_with_a_bad_reason_fails() {
        let src = [("crates/tutti-rt/src/x.rs", "#[cfg(unix)]\nfn f() {}\n")];
        for reason in [
            "because",
            "temporary: core will fix it",
            "temporary: dsp moves it behind a module seam",
            "temporary: nobody moves it behind a module seam",
            "permanent: no",
        ] {
            let got = run_with(&src, &[("crates/tutti-rt/src/x.rs", 1, reason)]);
            assert!(
                got.iter().any(|l| l.contains("ALLOW row:")),
                "{reason}: {got:?}"
            );
        }
        assert!(
            run_with(
                &src,
                &[(
                    "crates/tutti-rt/src/x.rs",
                    1,
                    "permanent: the libc symbol only exists here"
                )]
            )
            .is_empty()
        );
    }

    #[test]
    fn a_row_listed_twice_fails() {
        let src = [("crates/tutti-rt/src/x.rs", "#[cfg(unix)]\nfn f() {}\n")];
        let row = ("crates/tutti-rt/src/x.rs", 1, GOOD);
        assert!(run_with(&src, &[row, row])[1].contains("listed twice"));
    }

    #[test]
    fn owners_follow_the_session_lists() {
        assert_eq!(owner("tutti-fixtures"), Some("core"));
        assert_eq!(owner("tutti-nodes"), Some("dsp"));
        assert_eq!(owner("tutti-midi-hardware"), Some("midi"));
        assert_eq!(owner("tutti-vst3-host"), Some("plugins"));
        assert_eq!(owner("tutti-plugin-server"), Some("plugins"));
        assert_eq!(owner("tutti-shared-plugin"), Some("plugins"));
        assert_eq!(owner("tutti-unknown"), None);
    }
}
