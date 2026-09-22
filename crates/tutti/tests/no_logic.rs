//! The one rule this crate has, enforced.
//!
//! `src/lib.rs` must contain nothing but `pub use` and documentation. That is
//! not a style preference — it is the difference between this crate and the
//! one that was deleted in `4b5bd2fd`.
//!
//! The package of this name at `9c75ec54` was the workspace **root** package
//! and held `TuttiEngine`, `TuttiGraph`, `TuttiEngineBuilder`, `TuttiDriver`,
//! `audio_io`, `midi_export` and an error type. `0a4adf68` moved that logic
//! into `bevy-tutti` and `4b5bd2fd` deleted the package, because two stacked
//! umbrellas where the lower one owns what the upper one needs is one too
//! many. Re-exporting other crates was never the problem.
//!
//! So the failure mode is specific and it is one `fn build()` away: someone
//! adds a small convenience constructor here because it is the crate that can
//! see every subsystem, and the reason this package can exist at all quietly
//! stops being true. This test is the cheapest possible guard against that,
//! and it names the alternative in its failure message.

/// The file, read at compile time, so this test cannot go stale against a
/// moved path.
const LIB: &str = include_str!("../src/lib.rs");

/// Lines that are neither blank, nor documentation, nor an attribute.
fn code_lines() -> Vec<(usize, &'static str)> {
    LIB.lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| {
            !l.is_empty() && !l.starts_with("//") && !l.starts_with("#!") && !l.starts_with("#[")
        })
        .collect()
}

/// The code lines, joined into whole statements.
///
/// Line-based checking is not enough: a `pub use a::{ .. }` spanning three
/// lines makes its continuations look like bare expressions, and an earlier
/// version of this test failed on exactly that. Statements are accumulated
/// until braces balance and the text ends in `;`, `{` or `}`.
fn statements() -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut buf = String::new();
    let mut depth = 0i32;

    for (n, line) in code_lines() {
        if buf.is_empty() {
            start = n;
        } else {
            buf.push(' ');
        }
        buf.push_str(line);
        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;

        if depth <= 0 && (line.ends_with(';') || line.ends_with('{') || line.ends_with('}')) {
            out.push((start, std::mem::take(&mut buf)));
            depth = 0;
        }
    }
    if !buf.is_empty() {
        out.push((start, buf));
    }
    out
}

/// **No item in this crate may be anything but a re-export or a module.**
///
/// Checked by shape rather than by keyword blacklist: every statement must
/// be a `pub use`, a `pub mod`, or a closing brace. A blacklist of
/// `fn `/`struct `/`impl ` would miss a type alias, a const, a macro or a
/// trait — and the point is not to catch a particular bad word but to keep
/// the file to one shape.
#[test]
fn the_facade_contains_no_code() {
    let offenders: Vec<_> = statements()
        .into_iter()
        .filter(|(_, s)| {
            let s = s.trim();
            !(s.starts_with("pub use ") || s.starts_with("pub mod ") || s == "}")
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "crates/tutti/src/lib.rs must contain only `pub use`, `pub mod` and \
         documentation. Found:\n{}\n\nThis crate exists ONLY because it holds \
         no logic — the package of this name that did was deleted in \
         4b5bd2fd. Whatever this wants to be belongs in the crate that owns \
         the thing it wires: device bootstrap in tutti-cpal, graph logic in \
         tutti-core. See the module docs.",
        offenders
            .iter()
            .map(|(n, s)| format!("  line {n}: {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// **Every whole-crate re-export must be aliased.**
///
/// A bare `pub use tutti_core;` would let a consumer write
/// `tutti::tutti_core::Engine` — the redundant path
/// `scripts/check-canonical-paths.sh` exists to prevent, in a shape its
/// chained-path regex cannot see: that regex requires an underscore-bearing
/// crate segment on both sides, and `tutti::` has none. Aliasing removes the
/// spelling from the language rather than asking anyone to avoid it.
///
/// (The script carries its own copy of this check, for a reader who looks at
/// the gates rather than the tests.)
#[test]
fn every_crate_reexport_is_aliased() {
    let bare: Vec<_> = code_lines()
        .into_iter()
        .filter(|(_, l)| {
            l.starts_with("pub use tutti_")
                && !l.contains(" as ")
                // `pub use tutti_core::dsp;` and `::prelude::*` are module
                // re-exports, not crate ones; they carry no `tutti_` segment
                // into a consumer's path.
                && !l.contains("::")
        })
        .collect();

    assert!(
        bare.is_empty(),
        "a whole-crate re-export must be aliased (`as core`, `as sampler`, …) \
         so `tutti::tutti_core::…` is unspellable. Found:\n{}",
        bare.iter()
            .map(|(n, l)| format!("  line {n}: {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
