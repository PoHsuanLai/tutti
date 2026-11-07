//! A format under test, and the one case it may fail.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_plugin_types::Loaded;

/// Where in a block a format places a parameter point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamTiming {
    /// At the point's own sample offset.
    SampleAccurate,
    /// At the head of the block, whatever the offset.
    BlockHead,
}

/// A case a format cannot pass, and the text its failure must carry.
///
/// The case still runs. It must fail with `fails_with`, so a gap that stops
/// being true, or a case that starts failing for another reason, fails the
/// test. Keep `fails_with` specific.
///
/// A gap is a failure that unwinds. A case that aborts the process cannot be
/// one: `process_does_not_allocate` aborts under `AllocDisabler`, taking the
/// whole test binary with it under plain `cargo test`, so a format that
/// allocates in `process` fails it outright.
#[derive(Clone, Copy, Debug)]
pub struct Gap {
    /// Why the format cannot pass it.
    pub why: &'static str,
    /// Text the failure message contains.
    pub fails_with: &'static str,
}

/// One format's side of the contract: how to load the plugin, and what the
/// format cannot do.
pub trait Format {
    /// For failure messages.
    const NAME: &'static str;
    /// The loaded type.
    type L: Loaded;
    /// Where a parameter point lands.
    const PARAM_TIMING: ParamTiming;
    /// The cases this format fails, by name. [`contract!`](crate::contract)
    /// is given the number, so adding one is a second edit.
    const GAPS: &'static [(&'static str, Gap)];
    /// Loads a fresh instance of the shared plugin.
    fn load() -> Self::L;

    /// The plugin's own bytes (they begin with its magic) inside the state the
    /// host saved. The default assumes the host stores the plugin's bytes
    /// verbatim somewhere in its state; a format that encodes state (an AU
    /// property list, say) overrides this and [`embed_blob`](Self::embed_blob)
    /// to decode and encode it.
    fn extract_blob(state: &[u8]) -> Vec<u8> {
        crate::find_blob(state)
    }

    /// `state` with the plugin's bytes replaced by `blob`, which has the length
    /// [`extract_blob`](Self::extract_blob) returned. Default: the verbatim
    /// counterpart of the default `extract_blob`.
    fn embed_blob(state: &[u8], blob: &[u8]) -> Vec<u8> {
        crate::replace_blob(state, blob)
    }
}

fn message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("(not a text panic)")
}

/// Runs `body` as the case `name` of `F`: plainly, or, for a gap, requiring
/// that it fail with the gap's text.
///
/// # Panics
///
/// When the case fails, or a gapped case passes or fails for another reason.
pub fn run<F: Format>(name: &str, body: impl FnOnce()) {
    run_gapped(F::NAME, F::GAPS, name, body);
}

fn run_gapped(format: &str, gaps: &[(&str, Gap)], name: &str, body: impl FnOnce()) {
    let Some((_, gap)) = gaps.iter().find(|(case, _)| *case == name) else {
        return body();
    };
    let failure = catch_unwind(AssertUnwindSafe(body))
        .expect_err("claims a gap but passes: remove the claim");
    let got = message(failure.as_ref());
    assert!(
        got.contains(gap.fails_with),
        "{format} {name}: gap ({}) expects a failure containing {:?}, got {got:?}",
        gap.why,
        gap.fails_with,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAPS: &[(&str, Gap)] = &[(
        "case",
        Gap {
            why: "the format lacks it",
            fails_with: "got 0.5",
        },
    )];

    fn panic_message(f: impl FnOnce()) -> String {
        let failure = catch_unwind(AssertUnwindSafe(f)).expect_err("panics");
        message(failure.as_ref()).to_owned()
    }

    // Mutation (run), for the four tests: `run_gapped` treating every case as
    // gapped fails the first; not requiring the gapped body to fail fails the
    // second; ignoring the failure text fails the third.
    #[test]
    fn an_ungapped_case_runs_plainly() {
        run_gapped("f", GAPS, "other", || {});
        assert!(
            panic_message(|| run_gapped("f", GAPS, "other", || panic!("boom"))).contains("boom")
        );
    }

    #[test]
    fn a_gapped_case_that_fails_with_the_text_passes() {
        run_gapped("f", GAPS, "case", || panic!("level got 0.5"));
    }

    #[test]
    fn a_gapped_case_that_passes_is_refused() {
        let got = panic_message(|| run_gapped("f", GAPS, "case", || {}));
        assert!(got.contains("claims a gap but passes"), "{got}");
    }

    #[test]
    fn a_gapped_case_that_fails_for_another_reason_is_refused() {
        let got = panic_message(|| run_gapped("f", GAPS, "case", || panic!("something else")));
        assert!(got.contains("expects a failure containing"), "{got}");
    }
}
