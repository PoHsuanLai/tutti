//! `check-doc-idents`: a backticked item name in prose names an item the
//! workspace's code uses.
//!
//! The gate reads the [prose](super::prose) of the repo: the comments of every
//! `.rs` file and the text of the `.md` files outside `docs/design/` and
//! `docs/archive/`. In it, a backticked span (outside a fenced block) that is
//! a Rust path, optionally followed by `()`, `!` or generics, is an item name
//! when it is:
//!
//! - one `CamelCase` word (an uppercase first letter, a lowercase letter, at
//!   least three letters) or one `SCREAMING_SNAKE` word with an underscore;
//! - a `::` path of two or more identifiers, rooted anywhere but `std`,
//!   `core`, `alloc`, `cargo` (a build script's directives) or a crate that a manifest names as a dependency.
//!
//! The leading `crate`, `self`, `super` and `Self` of a path are skipped. An
//! item name resolves when each of its identifiers is a token of the code of
//! every `.rs` file under `crates/`, `testing/` and `xtask/` (comments and
//! the insides of literals blanked; a `SCREAMING_SNAKE` identifier also
//! resolves inside a literal, where an environment variable is read), or the
//! name of a package. A lowercase
//! word alone is never an item name: it is as often an English word.
//!
//! [`ALLOW`] lists the names that are not the workspace's to define (an SDK's
//! constants and interfaces in the crates that wrap it), each with its file
//! prefix and its reason. [`BASELINE`] holds the count of unresolved names
//! each file had when the gate began; a file fails when it holds more than its
//! baseline, and the report says how far the baseline can be lowered.
//!
//! Limits: a name resolves by its identifiers, not by its path, so
//! `module::item` passes when both exist anywhere in the workspace; a span
//! that a line break splits is not read.

use super::lex::{blank, comments};
use super::prose::{prose, word};
use super::ratchet::{self, Found, Wording};
use super::{Report, Tree};
use std::collections::BTreeSet;

/// `<count> <path>` lines: each file's unresolved names when the gate began.
pub(crate) const BASELINE: &str = "xtask/doc-idents.txt";

const WORDING: Wording = Wording {
    gate: "check-doc-idents",
    baseline: BASELINE,
    noun: "unresolved names",
    headline: "prose names an item no code uses; name the real one",
};

const CODE_ROOTS: [&str; 3] = ["crates", "testing", "xtask"];

/// A name the workspace does not define: the files that may say it (path
/// prefixes; `""` is every file), the names (tested against the first
/// identifier that does not resolve), and why.
pub(super) type Allow = (&'static [&'static str], fn(&str) -> bool, &'static str);

/// `s` is `prefix` followed by an uppercase letter.
fn camel_after(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_uppercase()))
}

/// `s` is one of `prefixes` followed by more text.
fn starts(s: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| s.starts_with(p))
}

pub(super) const ALLOW: &[Allow] = &[
    (&[""], |s| s == "NaN", "the IEEE 754 value, not an item"),
    (
        &[""],
        |s| {
            starts(
                s,
                &[
                    "SO_",
                    "O_",
                    "SCHED_",
                    "RLIMIT_",
                    "CAP_",
                    "RTLD_",
                    "PIPE_",
                    "ERROR_",
                    "WAIT_",
                    "THREAD_PRIORITY_",
                    "CONFIG_",
                    "DEP_",
                    "CARGO_",
                    "INT32_",
                    "UINT16_",
                    "WAVE_",
                    "KSAUDIO_",
                    "MPEG_",
                ],
            ) || matches!(
                s,
                "OUT_DIR"
                    | "ABOVE_NORMAL"
                    | "GetEnvironmentVariableA"
                    | "LoadLibrary"
                    | "SwitchToThread"
                    | "ReadFile"
                    | "CoInitialize"
                    | "MsgWaitForMultipleObjectsEx"
            )
        },
        "operating-system, C-header and cargo constants and functions (`SO_RCVTIMEO`, `CAP_SYS_NICE`, `WAVE_FORMAT_EXTENSIBLE`, `OUT_DIR`, `LoadLibrary`)",
    ),
    (
        &[""],
        |s| {
            ["NS", "CG", "CF", "UI", "AU", "MIDI", "MusicDevice"]
                .iter()
                .any(|p| camel_after(s, p))
                || starts(
                    s,
                    &[
                        "UInt",
                        "SInt",
                        "MIDI",
                        "CA_",
                        "CF_",
                        "DISPATCH_",
                        "AudioUnit",
                        "AudioComponent",
                    ],
                )
                || starts(
                    s,
                    &["AudioConvert", "AudioObject", "AudioTimeStamp", "mHostTime"],
                )
        },
        "Apple SDK types and functions (`NSView`, `UInt32`, `MIDIPacketNext`, `AudioUnitRender`, `CA_REALTIME_API`)",
    ),
    (
        &[
            "crates/tutti-vst2-",
            "testing/tutti-vst2-",
            "crates/tutti-plugin/",
            "crates/tutti-plugin-host/",
        ],
        |s| camel_after(s, "Vst") || starts(s, &["eff", "audioMaster", "AEff"]) || s == "ERect",
        "VST 2 SDK opcodes (`effOpen`, `audioMasterVersion`) and structs (`VstTimeInfo`, `AEffect`)",
    ),
    (
        &[
            "crates/tutti-vst3-",
            "crates/tutti-plugin-types/",
            "testing/tutti-shared-plugin",
        ],
        |s| camel_after(s, "I"),
        "VST 3 SDK COM interfaces (`IComponent`, `IPluginFactory`)",
    ),
    (
        &["crates/tutti-clap-", "testing/tutti-clap-test-plugin/"],
        |s| s.starts_with("CLAP_") || s.starts_with("clap_"),
        "CLAP SDK extension ids (`CLAP_EXT_*`) and C structs (`clap_host`)",
    ),
    (
        &["crates/tutti-alsa-ffi/", "crates/tutti-midi-hardware/"],
        |s| s.starts_with("snd_") || s.starts_with("SND_"),
        "ALSA sequencer C functions (`snd_seq_open`) and constants",
    ),
    (
        &["docs/reference/"],
        |_| true,
        "the SDK interface inventories name the SDKs' items, not the workspace's",
    ),
];

fn allowed(path: &str, ident: &str) -> bool {
    ALLOW
        .iter()
        .any(|(files, name, _)| files.iter().any(|f| path.starts_with(f)) && name(ident))
}

fn skipped(dir: &str) -> bool {
    matches!(dir, "target" | ".git" | "node_modules")
}

/// Every identifier of `blanked` code, plus the package names of the
/// manifests, and the names the manifests list as dependencies.
struct Known {
    tokens: BTreeSet<String>,
    /// The identifiers of the code with its string literals kept: the
    /// environment variables and C names a program reads by string.
    literal_tokens: BTreeSet<String>,
    foreign: BTreeSet<String>,
}

fn tokens_of(blanked: &str, into: &mut BTreeSet<String>) {
    let b = blanked.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let s = i;
            while i < b.len() && word(b[i]) {
                i += 1;
            }
            if !into.contains(&blanked[s..i]) {
                into.insert(blanked[s..i].to_owned());
            }
        } else if b[i].is_ascii_digit() {
            // A number's suffix (`1u8`, `0x1f`) is no identifier.
            while i < b.len() && word(b[i]) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

/// The package name and the dependency names of one manifest, with `-` as `_`.
fn manifest_names(text: &str, packages: &mut BTreeSet<String>, deps: &mut BTreeSet<String>) {
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(h) = line.strip_prefix('[') {
            section = h.trim_end_matches(']').trim_matches('[').to_owned();
            if let Some(dep) = section
                .split_once("dependencies.")
                .map(|(_, d)| d.to_owned())
            {
                deps.insert(dep.replace('-', "_"));
            }
            continue;
        }
        let key = line
            .split(['=', '.', ' '])
            .next()
            .unwrap_or("")
            .trim_matches('"');
        if section == "package" && key == "name" {
            if let Some(v) = line.split('"').nth(1) {
                packages.insert(v.replace('-', "_"));
            }
        } else if section.ends_with("dependencies") && !key.is_empty() && !key.starts_with('#') {
            deps.insert(key.replace('-', "_"));
        }
    }
}

/// `src` with its comments blanked and its literals kept.
fn uncommented(src: &str) -> String {
    let prose = comments(src);
    let bytes: Vec<u8> = src
        .bytes()
        .zip(prose.bytes())
        .map(|(c, p)| if p == b' ' || p == b'\n' { c } else { b' ' })
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn known(tree: &dyn Tree) -> Known {
    let mut tokens = BTreeSet::new();
    let mut literal_tokens = BTreeSet::new();
    let mut packages = BTreeSet::new();
    let mut deps = BTreeSet::new();
    for root in CODE_ROOTS {
        for path in tree.files(root, &skipped) {
            let Some(text) = tree.read(&path) else {
                continue;
            };
            if path.ends_with(".rs") {
                tokens_of(&blank(&text), &mut tokens);
                tokens_of(&uncommented(&text), &mut literal_tokens);
            } else if path.ends_with("Cargo.toml") {
                manifest_names(&text, &mut packages, &mut deps);
            }
        }
    }
    if let Some(text) = tree.read("Cargo.toml") {
        manifest_names(&text, &mut packages, &mut deps);
    }
    let foreign = deps.difference(&packages).cloned().collect();
    tokens.extend(packages);
    Known {
        tokens,
        literal_tokens,
        foreign,
    }
}

/// The backticked spans of `text` outside fenced blocks: (line, span).
fn spans(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (n, line) in text.lines().enumerate() {
        let body = line
            .trim_start()
            .trim_start_matches(['/', '!', '*'])
            .trim_start();
        if body.starts_with("```") || body.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let b = line.as_bytes();
        let run = |at: usize| b[at..].iter().take_while(|&&c| c == b'`').count();
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'`' {
                i += 1;
                continue;
            }
            let open = run(i);
            let from = i + open;
            let mut j = from;
            let mut close = None;
            while j < b.len() {
                if b[j] == b'`' {
                    let r = run(j);
                    if r == open {
                        close = Some(j);
                        break;
                    }
                    j += r;
                } else {
                    j += 1;
                }
            }
            let Some(end) = close else { break };
            out.push((n + 1, line[from..end].trim()));
            i = end + open;
        }
    }
    out
}

fn ident_at(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if !b
        .first()
        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
    {
        return None;
    }
    let n = b.iter().take_while(|&&c| word(c)).count();
    Some(&s[..n])
}

fn camel(s: &str) -> bool {
    s.len() >= 3
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars().any(|c| c.is_ascii_lowercase())
        && !s.contains('_')
}

fn screaming(s: &str) -> bool {
    s.contains('_')
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn snake(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The identifiers of a span that is an item name, or `None`.
fn item_name(span: &str) -> Option<Vec<&str>> {
    let mut rest = span;
    let mut segs = Vec::new();
    loop {
        let id = ident_at(rest)?;
        segs.push(id);
        rest = &rest[id.len()..];
        match rest.strip_prefix("::") {
            Some(next) if ident_at(next).is_some() => rest = next,
            _ => break,
        }
    }
    let tail_ok = rest.is_empty()
        || rest == "::*"
        || rest.starts_with(['(', '<', '!'])
        || rest.starts_with("::<");
    if !tail_ok {
        return None;
    }
    while segs.len() > 1 && matches!(segs[0], "crate" | "self" | "super" | "Self") {
        segs.remove(0);
    }
    if matches!(segs[0], "crate" | "self" | "super" | "Self") {
        return None;
    }
    let shaped = match segs.as_slice() {
        [one] => camel(one) || screaming(one),
        many => many.iter().any(|s| camel(s)) || many.iter().all(|s| snake(s) || screaming(s)),
    };
    shaped.then_some(segs)
}

/// The first identifier of `segs` that no code of the workspace uses; `None`
/// when the name resolves or is rooted at a foreign crate.
fn unresolved<'a>(segs: &[&'a str], known: &Known) -> Option<&'a str> {
    if segs.len() > 1
        && (matches!(segs[0], "std" | "core" | "alloc" | "cargo")
            || known.foreign.contains(segs[0]))
    {
        return None;
    }
    segs.iter()
        .copied()
        .find(|s| !(known.tokens.contains(*s) || screaming(s) && known.literal_tokens.contains(*s)))
}

pub(super) fn check(tree: &dyn Tree) -> Report {
    let baseline = match ratchet::load(tree, &WORDING) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let known = known(tree);
    let mut found = Vec::new();
    for p in prose(tree) {
        let mut sites = Vec::new();
        for (line, span) in spans(&p.text) {
            let Some(segs) = item_name(span) else {
                continue;
            };
            let Some(bad) = unresolved(&segs, &known) else {
                continue;
            };
            if !allowed(&p.path, bad) {
                sites.push(format!(
                    "    {}:{line}: `{span}` names no item (`{bad}` is in no code)",
                    p.path
                ));
            }
        }
        found.push(Found {
            path: p.path,
            sites,
        });
    }
    ratchet::settle(&WORDING, &baseline, found)
}

#[cfg(test)]
mod tests {
    use super::super::fake::Mem;
    use super::*;

    fn run(files: &[(&str, &str)]) -> Report {
        let mut all = vec![(BASELINE, "")];
        all.extend_from_slice(files);
        check(&Mem::with(&all))
    }

    #[test]
    fn a_name_no_code_uses_fails_with_its_file_and_line() {
        let r = run(&[(
            "crates/a/src/lib.rs",
            "struct Real;\n// the `Real` type, not the `Gone` one\n/// see `Real::new` and `Real::gone()`\nfn new() {}\n",
        )]);
        assert_eq!(
            r.findings,
            [
                "check-doc-idents: FAIL (prose names an item no code uses; name the real one):",
                "  crates/a/src/lib.rs: 2 unresolved names, baseline 0",
                "    crates/a/src/lib.rs:2: `Gone` names no item (`Gone` is in no code)",
                "    crates/a/src/lib.rs:3: `Real::gone()` names no item (`gone` is in no code)",
            ]
        );
    }

    #[test]
    fn a_name_that_code_uses_resolves_even_when_only_a_comment_or_string_says_the_word() {
        let r = run(&[(
            "crates/a/src/lib.rs",
            "// `Ghost` and `Phantom`\nconst S: &str = \"Ghost\";\nstruct Phantom;\n",
        )]);
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
        assert!(r.findings[2].contains("`Ghost`"));
    }

    #[test]
    fn markdown_prose_is_read_but_fenced_blocks_and_the_records_are_not() {
        let r = run(&[
            ("crates/a/src/lib.rs", "struct Real;\n"),
            (
                "crates/a/README.md",
                "A `Real` and a `Fake`.\n```rust\nlet x = `Hidden`;\n```\n",
            ),
            ("docs/design/001-x.md", "`Record`\n"),
            ("docs/archive/x.md", "`Archived`\n"),
        ]);
        assert_eq!(
            r.findings[1..],
            [
                "  crates/a/README.md: 1 unresolved names, baseline 0",
                "    crates/a/README.md:1: `Fake` names no item (`Fake` is in no code)",
            ]
        );
    }

    #[test]
    fn only_item_shaped_spans_are_names() {
        let names = |s: &str| item_name(s).map(|v| v.join("::"));
        assert_eq!(names("Foo"), Some("Foo".into()));
        assert_eq!(names("Foo::bar()"), Some("Foo::bar".into()));
        assert_eq!(names("Vec<Foo>"), Some("Vec".into()));
        assert_eq!(names("a::b"), Some("a::b".into()));
        assert_eq!(names("crate::a::Bar"), Some("a::Bar".into()));
        assert_eq!(names("MAX_TAIL"), Some("MAX_TAIL".into()));
        for not in [
            "word", "L2", "Ok", "x.y", "Foo bar", "foo_bar", "MIDI", "Self", "self::x", "1.0",
            "-Foo", "a::", "Foo.bar",
        ] {
            assert_eq!(item_name(not), None, "{not}");
        }
    }

    #[test]
    fn std_and_dependency_rooted_paths_are_not_checked_but_workspace_ones_are() {
        let r = run(&[
            (
                "crates/a/Cargo.toml",
                "[package]\nname = \"tutti-a\"\n[dependencies]\ncpal = \"1\"\n[dependencies.symphonia]\nversion = \"1\"\n",
            ),
            (
                "crates/a/src/lib.rs",
                "// `std::sync::Atomic` `cpal::Stream` `symphonia::Probe` `tutti_a::Gone`\n",
            ),
        ]);
        assert_eq!(r.findings.len(), 3, "{:?}", r.findings);
        assert!(r.findings[2].contains("`tutti_a::Gone`"));
    }

    #[test]
    fn an_sdk_name_is_allowed_only_in_the_crate_that_wraps_the_sdk() {
        let r = run(&[
            (
                "crates/tutti-vst3-ffi/src/lib.rs",
                "// `IComponent` `Gone`\n",
            ),
            ("crates/tutti-graph/src/lib.rs", "// `IComponent`\n"),
        ]);
        assert_eq!(
            r.findings[1..],
            [
                "  crates/tutti-graph/src/lib.rs: 1 unresolved names, baseline 0",
                "    crates/tutti-graph/src/lib.rs:1: `IComponent` names no item (`IComponent` is in no code)",
                "  crates/tutti-vst3-ffi/src/lib.rs: 1 unresolved names, baseline 0",
                "    crates/tutti-vst3-ffi/src/lib.rs:1: `Gone` names no item (`Gone` is in no code)",
            ]
        );
    }

    #[test]
    fn a_file_within_its_baseline_passes_and_a_missing_baseline_fails() {
        let files = [("crates/a/src/lib.rs", "// `Gone`\n")];
        let ok = check(&Mem::with(&[
            (BASELINE, "1 crates/a/src/lib.rs\n"),
            files[0],
        ]));
        assert!(ok.passed());
        assert!(!check(&Mem::with(&files)).passed());
    }
}
