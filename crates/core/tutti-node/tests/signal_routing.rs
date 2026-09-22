//! `Signal`, `SignalFrame` and `Routing` — the vocabulary [`AudioUnit::route`]
//! speaks — exercised from outside the crate.
//!
//! `src/signal.rs` had no test of its own, which is worth stating plainly
//! because of what it decides: a `Signal` is how a latency crosses a node, and
//! `AudioUnit::latency` is nothing but `route` run over a frame of
//! `Signal::Latency(0.0)` with the minimum read back out. Plugin delay
//! compensation is computed from that number. Arithmetic that is wrong by a
//! handful of samples does not fail anywhere — it shifts one track against the
//! others and presents as "the session drifts", which is why the assertions
//! below compare against a figure the test itself adds up rather than against a
//! second call into the same code.
//!
//! **These tests found four defects, and all four are now fixed.** They were
//! landed first as characterizations — named and documented as wrong, marked
//! delete-or-rewrite rather than update-in-place — and the fixes followed in
//! the same branch, so no version of `main` ever carried a test blessing the
//! old behaviour. What they were:
//!
//! - `Routing::Arbitrary` added the node's own latency **once per input**, so
//!   the reported latency depended on the order the input channels were wired.
//! - `Routing::Generator` answered `Unknown` for the zero-input generator that
//!   is its whole reason to exist: the empty-input guard ran before the match.
//! - The same guard swallowed `Routing::Reverse`'s `assert_eq!`. That one is
//!   *not* treated as a defect — see
//!   `an_empty_input_frame_short_circuits_every_variant_but_the_generator` for
//!   why an empty frame is legitimately "no information" there.
//! - `Routing::Join` panicked on more outputs than inputs and on zero outputs,
//!   both reachable from signal analysis rather than from audio processing.
//!
//! Every one of them was inert in the tree as it stood, because each in-tree
//! caller happened to pass the arguments that make the wrong answer and the
//! right one coincide. That is why they survived, and why the tests use
//! non-zero latencies and uneven widths where the old code's coincidences
//! would otherwise hide a regression.
//!
//! `Signal` derives neither `Debug` nor `PartialEq`, so every assertion goes
//! through the small destructuring helpers at the top of the file. They panic
//! with the variant they actually saw, which is the whole reason they exist —
//! `assert!(matches!(..))` reports only that a match failed.
//!
//! [`AudioUnit::route`]: tutti_node::AudioUnit::route

use core::cell::Cell;

// `num-complex` is a direct dependency of `tutti-node` (a `Signal::Response`
// carries a `Complex64`) but the crate re-exports neither the crate nor the
// type, so there is no root spelling to prefer here.
use num_complex::Complex64;
use tutti_node::{Routing, Signal, SignalFrame};

/// The variant name and payload, for an assertion message. `Signal` has no
/// `Debug`, and a failure that says only "expected Latency" leaves the reader
/// re-running the test under a debugger to learn what it got instead.
fn describe(signal: Signal) -> String {
    match signal {
        Signal::Unknown => "Unknown".to_string(),
        Signal::Value(v) => format!("Value({v})"),
        Signal::Latency(l) => format!("Latency({l})"),
        Signal::Response(r, l) => format!("Response({r}, {l})"),
    }
}

fn latency_of(signal: Signal) -> f64 {
    match signal {
        Signal::Latency(l) => l,
        other => panic!("expected Signal::Latency, got {}", describe(other)),
    }
}

fn value_of(signal: Signal) -> f64 {
    match signal {
        Signal::Value(v) => v,
        other => panic!("expected Signal::Value, got {}", describe(other)),
    }
}

fn response_of(signal: Signal) -> (Complex64, f64) {
    match signal {
        Signal::Response(r, l) => (r, l),
        other => panic!("expected Signal::Response, got {}", describe(other)),
    }
}

fn assert_unknown(signal: Signal, what: &str) {
    if !matches!(signal, Signal::Unknown) {
        panic!("{what}: expected Signal::Unknown, got {}", describe(signal));
    }
}

/// A frame built channel by channel, which is the only way to build a populated
/// one — `SignalFrame::new` fills with `Unknown` and there is no `from_slice`.
fn frame(channels: &[Signal]) -> SignalFrame {
    let mut f = SignalFrame::new(channels.len());
    for (i, signal) in channels.iter().enumerate() {
        f.set(i, *signal);
    }
    f
}

/// The identity frequency response, for the cases where `filter` is under test
/// for its latency arithmetic rather than for what it does to the response.
fn pass_through(response: Complex64) -> Complex64 {
    response
}

// ---------------------------------------------------------------------------
// Signal: latency arithmetic
// ---------------------------------------------------------------------------

/// Latency accumulates additively along a chain, and it does so identically for
/// `Latency` and for `Response` — the two variants that carry one.
///
/// This is the property plugin delay compensation rests on. The expected figure
/// is summed from `HOPS` by the test, not obtained by calling `delay`/`filter`
/// again, so an off-by-one in any single arm cannot cancel itself out. The
/// `Response` half runs the same hops through the same methods and must land on
/// the same total: a divergence between the two would mean a graph's reported
/// latency depended on whether anything downstream had asked for a frequency
/// response, which nothing would ever catch in production.
///
/// Mutation run: in `Signal::delay`, `Signal::Latency(l + latency)` →
/// `Signal::Latency(*l)`. Failed, as expected (48 instead of 138). Also
/// `Signal::filter`'s `Signal::Response(filter(*response), l + latency)` →
/// `Signal::Response(filter(*response), *l)`: failed (106 instead of 138).
#[test]
fn latency_accumulates_additively_along_a_chain() {
    // delay, filter, distort, delay — every method on `Signal` that takes a
    // latency except the two combiners, which have their own test.
    const HOPS: [f64; 4] = [64.0, 32.0, 16.0, 26.0];
    let total: f64 = HOPS.iter().sum();

    let chained = Signal::Latency(0.0)
        .delay(HOPS[0])
        .filter(HOPS[1], pass_through)
        .distort(HOPS[2])
        .delay(HOPS[3]);
    assert_eq!(
        latency_of(chained),
        total,
        "a latency chained through delay/filter/distort/delay must equal the sum of the hops"
    );

    // Same hops, starting from a signal that also carries a response. `distort`
    // erases the response and keeps the latency, so this ends as a `Latency`
    // too, and the total must not differ.
    let with_response = Signal::Response(Complex64::new(1.0, 0.0), 0.0)
        .delay(HOPS[0])
        .filter(HOPS[1], pass_through)
        .distort(HOPS[2])
        .delay(HOPS[3]);
    assert_eq!(
        latency_of(with_response),
        total,
        "a Response must accumulate the same latency a bare Latency does"
    );

    // A non-zero starting latency is carried, not replaced: a node downstream
    // of a 512-sample lookahead limiter must report 512 plus its own.
    assert_eq!(
        latency_of(Signal::Latency(512.0).delay(HOPS[0])),
        512.0 + HOPS[0]
    );
}

/// A signal that carries no latency never acquires one, however much latency is
/// passed to the method that processes it.
///
/// `Value` and `Unknown` have no latency field, so the only two honest answers
/// are "unchanged" and "Unknown" — and the code picks a different one per
/// method. `delay` returns the input untouched (a delayed constant is the same
/// constant); `filter` and `distort` return `Unknown`. The asymmetry is the
/// point: a constant that has been through a nonlinearity is no longer known to
/// be constant, but one that has merely been delayed still is. Getting this
/// backwards would either invent a latency for a DC source — which PDC would
/// then compensate for — or throw away the constant-folding that
/// `AudioUnit::response` depends on.
///
/// Mutation run: in `Signal::delay`, the catch-all `x => *x` → `_ =>
/// Signal::Latency(latency)`. Failed, as expected.
#[test]
fn a_signal_without_a_latency_never_acquires_one() {
    let dc = Signal::Value(0.25);

    assert_eq!(
        value_of(dc.delay(64.0)),
        0.25,
        "delaying a constant leaves the same constant — there is nothing for a delay to change"
    );
    assert_eq!(
        value_of(dc.scale(4.0)),
        1.0,
        "scale folds through a constant"
    );

    // Both of these discard the value rather than keep it: see the doc comment.
    assert_unknown(dc.filter(64.0, pass_through), "Value filtered");
    assert_unknown(dc.distort(64.0), "Value distorted");

    // `Unknown` absorbs everything. In particular it does not become
    // `Latency(64.0)` — an unknown signal has not been shown to have zero
    // latency either, and `AudioUnit::latency` skips it for exactly that reason.
    assert_unknown(Signal::Unknown.delay(64.0), "Unknown delayed");
    assert_unknown(
        Signal::Unknown.filter(64.0, pass_through),
        "Unknown filtered",
    );
    assert_unknown(Signal::Unknown.distort(64.0), "Unknown distorted");
    assert_unknown(Signal::Unknown.scale(4.0), "Unknown scaled");
}

/// `scale` reaches the value and the frequency response and never the latency —
/// including at `factor == 0.0`, where scaling a signal to silence leaves its
/// latency exactly where it was.
///
/// A gain stage is the one processor in the engine that is guaranteed *not* to
/// move a signal in time, so `scale` touching a latency would be a pure defect.
/// The zero case is the one to pin, because "multiply by zero" is where a
/// plausible-looking simplification (collapse to `Value(0.0)`, since the output
/// is silent) would land; taking it would erase the latency of a muted path,
/// and `Routing::Join` — which calls `scale` on every output it produces —
/// would start reporting a normalized bundle as having no latency at all.
///
/// Mutation run: in `Signal::scale`, `Signal::Response(response * factor,
/// *latency)` → `Signal::Response(*response, *latency)`. Failed, as expected.
/// Also the catch-all `x => *x` → `_ => Signal::Value(0.0)`: failed on the
/// `Latency` assertion.
#[test]
fn scale_reaches_the_value_and_the_response_but_never_the_latency() {
    assert_eq!(value_of(Signal::Value(3.0).scale(2.0)), 6.0);
    assert_eq!(
        value_of(Signal::Value(3.0).scale(0.0)),
        0.0,
        "a constant scaled by zero is the zero constant, not Unknown"
    );

    let (response, latency) =
        response_of(Signal::Response(Complex64::new(2.0, -1.0), 7.0).scale(3.0));
    assert_eq!(response, Complex64::new(6.0, -3.0));
    assert_eq!(latency, 7.0, "scaling a response must not move it in time");

    let (response, latency) =
        response_of(Signal::Response(Complex64::new(2.0, -1.0), 7.0).scale(0.0));
    assert_eq!(response, Complex64::new(0.0, 0.0));
    assert_eq!(
        latency, 7.0,
        "a response scaled to silence keeps its latency — a muted path is still a delayed path"
    );

    assert_eq!(
        latency_of(Signal::Latency(7.0).scale(0.0)),
        7.0,
        "a latency has no value to scale, so scale(0.0) is the identity on it"
    );
}

/// Both combiners take the **earlier** of two latencies, and do so whichever
/// side it arrives on.
///
/// `min` rather than `max` is the deliberate choice, and it matches what
/// `AudioUnit::latency` does across outputs: the reported latency is the point
/// at which the output *starts* responding to its input, so a summing node that
/// mixes a dry path with a 512-sample lookahead path responds immediately.
/// Symmetry matters separately from the choice — the two arguments of a mix are
/// not ordered, so a combiner whose answer depended on which operand was `self`
/// would make a graph's latency depend on the order the edges were declared in.
/// (`Routing::Arbitrary` violates exactly that; see
/// `arbitrary_reports_a_different_latency_when_the_channels_are_reordered`.)
///
/// Mutation run: in `Signal::combine_linear`, the `(Latency, Latency)` arm's
/// `min(lx, ly) + latency` → `max(lx, ly) + latency`. Failed, as expected.
/// Separately, in `combine_nonlinear`, the same substitution on its
/// `(Latency, Latency)` arm: failed.
#[test]
fn both_combiners_take_the_earlier_of_two_latencies() {
    const EARLY: f64 = 2.0;
    const LATE: f64 = 10.0;
    const EXTRA: f64 = 5.0;

    let sum = |x: f64, y: f64| x + y;
    let sum_c = |x: Complex64, y: Complex64| x + y;

    for (label, (a, b)) in [
        ("late on the left", (LATE, EARLY)),
        ("early on the left", (EARLY, LATE)),
    ] {
        let nonlinear = Signal::Latency(a).combine_nonlinear(Signal::Latency(b), EXTRA);
        assert_eq!(
            latency_of(nonlinear),
            EARLY + EXTRA,
            "combine_nonlinear, {label}: the earlier latency plus the node's own must win"
        );

        let linear = Signal::Latency(a).combine_linear(Signal::Latency(b), EXTRA, sum, sum_c);
        assert_eq!(
            latency_of(linear),
            EARLY + EXTRA,
            "combine_linear, {label}: the earlier latency plus the node's own must win"
        );
    }

    // A `Response` mixed with a bare `Latency` cannot stay a response — the
    // other operand has none to combine with — so it degrades to the minimum
    // latency. The response is dropped, not carried through unchanged.
    let mixed = Signal::Response(Complex64::new(1.0, 0.0), LATE).combine_linear(
        Signal::Latency(EARLY),
        EXTRA,
        sum,
        sum_c,
    );
    assert_eq!(latency_of(mixed), EARLY + EXTRA);

    // An operand that carries no latency contributes none: `Value` distorts to
    // `Unknown` inside `combine_nonlinear`, so the surviving operand's latency
    // passes through even though it is the larger of the two numbers present.
    assert_eq!(
        latency_of(Signal::Latency(LATE).combine_nonlinear(Signal::Value(1.0), EXTRA)),
        LATE + EXTRA,
        "a constant is not a zero-latency signal; it drops out of the minimum entirely"
    );

    // Two constants through a nonlinearity is the one case with no latency to
    // report at all.
    assert_unknown(
        Signal::Value(1.0).combine_nonlinear(Signal::Value(2.0), EXTRA),
        "two constants combined nonlinearly",
    );
}

/// `combine_linear` feeds a constant operand into the response function as a
/// **zero** response, as its doc comment says, and folds two constants with the
/// value function instead.
///
/// This is what makes `AudioUnit::response` composable across a mixer: a DC
/// branch summed into a signal branch must contribute nothing to the frequency
/// response, since a constant has no response at any non-zero frequency. The
/// test captures the argument the closure actually received rather than
/// inferring it from the result, because `response(rx, 0)` and a hypothetical
/// `rx` passed straight through are indistinguishable for the `+` the callers
/// use — `Routing::Join` passes `|x, y| x + y`, so a bug here would be invisible
/// to every in-tree caller and would surface only once some node summed with a
/// different function.
///
/// Mutation run: in `Signal::combine_linear`, the `(Response, Value)` arm's
/// `Complex64::new(0.0, 0.0)` → `Complex64::new(1.0, 0.0)`. Failed, as expected
/// (the captured operand was 1 rather than 0).
#[test]
fn combine_linear_feeds_a_constant_in_as_a_zero_response() {
    let sum = |x: f64, y: f64| x + y;
    let signal = Complex64::new(2.0, 0.0);

    // Constant on the right.
    let seen = Cell::new(f64::NAN);
    let out =
        Signal::Response(signal, 10.0).combine_linear(Signal::Value(3.0), 5.0, sum, |x, y| {
            seen.set(y.re);
            x + y
        });
    assert_eq!(
        seen.get(),
        0.0,
        "the constant operand must arrive as a zero response"
    );
    let (response, latency) = response_of(out);
    assert_eq!(response, signal);
    assert_eq!(
        latency, 15.0,
        "the constant contributes no latency, so the response's own plus the node's extra survives"
    );

    // Constant on the left: same rule, mirrored.
    let seen = Cell::new(f64::NAN);
    let out =
        Signal::Value(3.0).combine_linear(Signal::Response(signal, 10.0), 5.0, sum, |x, y| {
            seen.set(x.re);
            x + y
        });
    assert_eq!(seen.get(), 0.0);
    assert_eq!(response_of(out).1, 15.0);

    // Two constants never reach the response function at all — they fold with
    // the value function, and the extra latency is discarded because the result
    // has nowhere to put it.
    let out = Signal::Value(3.0).combine_linear(
        Signal::Value(4.0),
        5.0,
        |x, y| x * y,
        |_, _| panic!("the response function must not be called for two constants"),
    );
    assert_eq!(value_of(out), 12.0);
}

// ---------------------------------------------------------------------------
// SignalFrame
// ---------------------------------------------------------------------------

/// `SignalFrame::copy(source, i, n)` takes the `n` channels **starting at** `i`
/// — not the first `n`, and not everything from `i` to the end.
///
/// The argument order invites reading it as `(start, end)`, and both misreads
/// produce a frame of the plausible length for at least some inputs, so nothing
/// downstream would notice: a node that split its input frame one channel late
/// would route the wrong source's latency to each of its outputs, and the graph
/// would compensate the wrong track. The frame here is populated with distinct
/// constants precisely so that a shifted window is a different answer rather
/// than the same answer.
///
/// Mutation run: `frame.0[0..n].copy_from_slice(&source.0[i..i + n])` →
/// `&source.0[i + 1..i + n + 1]`. Failed, as expected.
#[test]
fn copy_takes_n_channels_starting_at_i() {
    let source = frame(&[
        Signal::Value(0.0),
        Signal::Value(1.0),
        Signal::Value(2.0),
        Signal::Value(3.0),
    ]);

    let window = SignalFrame::copy(&source, 1, 2);
    assert_eq!(
        window.len(),
        2,
        "the new frame is `n` channels wide, not `source.len()`"
    );
    assert_eq!(value_of(window.at(0)), 1.0);
    assert_eq!(value_of(window.at(1)), 2.0);

    // The window may run to the last channel inclusive.
    let tail = SignalFrame::copy(&source, 2, 2);
    assert_eq!(value_of(tail.at(0)), 2.0);
    assert_eq!(value_of(tail.at(1)), 3.0);

    // Copying everything is the identity.
    let whole = SignalFrame::copy(&source, 0, 4);
    for i in 0..4 {
        assert_eq!(value_of(whole.at(i)), i as f64);
    }

    // An empty window starting one past the last channel is legal, which is
    // what lets a caller loop over bundles without special-casing the last one.
    assert!(SignalFrame::copy(&source, 4, 0).is_empty());
}

/// A window that runs past the end of the source panics rather than clamping or
/// padding with `Unknown`.
///
/// Worth pinning because the alternative is the quiet one: a `copy` that
/// silently returned a short or `Unknown`-padded frame would hand a node an
/// input channel that reports no latency, and `AudioUnit::latency` skips
/// `Unknown` when taking its minimum — so the node would report the latency of
/// its *remaining* channels and the graph would under-compensate with nothing
/// logged anywhere. The panic comes from the slice index in `copy`, not from an
/// explicit check, so this test is what documents that the behaviour is relied
/// on rather than incidental.
///
/// Mutation run: `&source.0[i..i + n]` → `&source.0[i..source.0.len().min(i + n)]`
/// (with the destination range narrowed to match). The test failed — it no
/// longer panicked — as expected.
#[test]
#[should_panic(expected = "range end index")]
fn copy_past_the_end_of_the_source_panics() {
    let source = frame(&[Signal::Value(0.0), Signal::Value(1.0), Signal::Value(2.0)]);
    let _ = SignalFrame::copy(&source, 2, 2);
}

/// `len` and `length` are two names for one number, and both are the channel
/// count.
///
/// Two near-identical names on one type is where a caller picks whichever
/// completes first and is never corrected, so the useful thing to pin is that
/// it does not matter — and that neither is, say, a byte count or the inline
/// capacity of the backing `TinyVec`, which is 16 and would agree with the
/// channel count for the first sixteen channels of every frame anyone tested by
/// hand. The frame below is deliberately wider than 16 so a capacity-shaped
/// answer is a different number.
///
/// Mutation run: `pub fn length(&self) -> usize { self.0.len() }` → `{ self.0.len() + 1 }`.
/// Failed, as expected. Separately, `len` → `{ 16 }`: failed.
#[test]
fn len_and_length_are_two_names_for_the_channel_count() {
    for channels in [0usize, 1, 16, 24] {
        let f = SignalFrame::new(channels);
        assert_eq!(f.len(), channels, "len() must be the channel count");
        assert_eq!(f.length(), channels, "length() must be the channel count");
        assert_eq!(f.is_empty(), channels == 0);
    }

    // And they keep agreeing after the frame is resized, which is the only way
    // the count ever changes.
    let mut f = SignalFrame::new(24);
    f.resize(3);
    assert_eq!(f.len(), 3);
    assert_eq!(f.length(), f.len());
}

/// `resize` fills new channels with `Unknown`, and a channel that was resized
/// away does not come back when the frame grows again.
///
/// The second half is the one that could plausibly be false: `SignalFrame` is a
/// `TinyVec` with sixteen inline slots, so shrinking and re-growing within that
/// inline array touches storage that was never freed. If a stale `Signal`
/// survived there, a node that reused a frame across a rebuild would read a
/// latency belonging to a channel that no longer exists — and because the stale
/// value is a *plausible* latency rather than a garbage one, no assertion
/// anywhere would fire. `Unknown` is the correct filler for the same reason
/// `SignalFrame::new` uses it: an unpopulated channel has not been shown to have
/// zero latency.
///
/// Mutation run: `self.0.resize(size, Signal::Unknown)` → `self.0.resize(size,
/// Signal::Latency(0.0))`. Failed, as expected.
#[test]
fn resize_fills_new_channels_with_unknown_and_does_not_resurrect_old_ones() {
    let mut f = frame(&[
        Signal::Value(0.0),
        Signal::Value(1.0),
        Signal::Value(2.0),
        Signal::Value(3.0),
    ]);

    f.resize(6);
    assert_eq!(f.len(), 6);
    assert_eq!(
        value_of(f.at(3)),
        3.0,
        "growing must not disturb the existing channels"
    );
    assert_unknown(f.at(4), "channel 4 after growing");
    assert_unknown(f.at(5), "channel 5 after growing");

    f.resize(2);
    assert_eq!(f.len(), 2);
    assert_eq!(
        value_of(f.at(1)),
        1.0,
        "shrinking must not disturb the surviving channels"
    );

    // Back up to the original width, entirely within the sixteen inline slots.
    f.resize(4);
    assert_unknown(f.at(2), "channel 2 after shrinking and re-growing");
    assert_unknown(f.at(3), "channel 3 after shrinking and re-growing");

    // `fill` overwrites every channel, including ones `resize` just created.
    f.fill(Signal::Latency(8.0));
    for i in 0..f.len() {
        assert_eq!(latency_of(f.at(i)), 8.0);
    }
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// `Routing::Split` fans inputs out to any number of outputs by wrapping, and
/// `Routing::Join` is its inverse — including the normalization that makes the
/// round trip an identity on constants.
///
/// The pairing is the load-bearing detail and it is **interleaved**, not
/// blocked: join's output `i` gathers inputs `i`, `i + outputs`,
/// `i + 2 * outputs`, … which is exactly the set split scattered there. The
/// constants below are chosen so the two readings give different answers — 1+3
/// and 2+4 halved is `[2, 3]`, whereas a blocked 1+2 and 3+4 halved would be
/// `[1.5, 3.5]` — because a blocked join would still produce a well-formed
/// frame of the right width and would only be audible as channels swapping
/// places in a multichannel bundle.
///
/// Mutation run: in `Routing::Join`, `input.at(i + j * outputs)` →
/// `input.at(i * bundle + j)`. Failed, as expected (the round trip returned
/// 1.5, not 1.0). In `Routing::Split`, `input.at(i % input.len())` →
/// `input.at(i % outputs)`: failed, by indexing past the end of the two-channel
/// input on the 2→5 case. And the two mutations that the sibling latency test
/// cannot see, because `scale` is the identity on a `Latency` — `combo.scale(…)`
/// → `combo.scale(0.0)` (returned 0.0) and the normalizing factor inverted to
/// `input.len() / output.len()` (returned 4.0): both failed here.
#[test]
fn split_wraps_around_and_join_inverts_it() {
    let stereo = frame(&[Signal::Value(1.0), Signal::Value(2.0)]);

    // More outputs than inputs: wrap.
    let wide = Routing::Split.route(&stereo, 5);
    assert_eq!(wide.len(), 5);
    let expected = [1.0, 2.0, 1.0, 2.0, 1.0];
    for (i, want) in expected.iter().enumerate() {
        assert_eq!(value_of(wide.at(i)), *want, "split channel {i}");
    }

    // Fewer outputs than inputs: the surplus inputs are simply dropped.
    let narrow = Routing::Split.route(&stereo, 1);
    assert_eq!(narrow.len(), 1);
    assert_eq!(value_of(narrow.at(0)), 1.0);

    // Zero outputs: an empty frame, no panic.
    assert!(Routing::Split.route(&stereo, 0).is_empty());

    // Split then join round-trips, which is what the `outputs / inputs` scale
    // factor in `Join` exists for.
    let split4 = Routing::Split.route(&stereo, 4);
    let rejoined = Routing::Join.route(&split4, 2);
    assert_eq!(rejoined.len(), 2);
    assert_eq!(
        value_of(rejoined.at(0)),
        1.0,
        "split-then-join must be the identity"
    );
    assert_eq!(value_of(rejoined.at(1)), 2.0);

    // The interleaved pairing, on four distinct constants.
    let quad = frame(&[
        Signal::Value(1.0),
        Signal::Value(2.0),
        Signal::Value(3.0),
        Signal::Value(4.0),
    ]);
    let joined = Routing::Join.route(&quad, 2);
    assert_eq!(
        value_of(joined.at(0)),
        2.0,
        "output 0 gathers inputs 0 and 2, halved"
    );
    assert_eq!(
        value_of(joined.at(1)),
        3.0,
        "output 1 gathers inputs 1 and 3, halved"
    );
}

/// A join reports the **earliest** latency in each bundle, and the normalizing
/// `scale` it applies afterwards does not disturb it.
///
/// This is the composition of two rules already pinned separately
/// (`combine_linear` takes the minimum; `scale` never touches a latency) at the
/// one call site in the crate that uses both, and it is the shape a real mixer
/// has: four sources at different latencies folded down to a stereo bus. Each
/// output must report when *it* first responds, and must report it per bundle
/// rather than taking a single minimum across the whole frame — an output whose
/// own sources are all late would otherwise claim to respond immediately.
///
/// Mutation run: in `Routing::Join`, `output.set(i, combo.scale(…))` →
/// `output.set(i, combo.scale(0.0))`. The test did **not** fail, and that is
/// recorded rather than papered over: `scale` is the identity on a `Latency`,
/// which is exactly the second half of what this test asserts, so no latency
/// assertion anywhere can see that mutation. It is killed instead by
/// `split_wraps_around_and_join_inverts_it`, which routes constants through the
/// same normalization — verified, not assumed. The mutation this test does kill:
/// `let mut combo = input.at(i)` → `let mut combo = input.at(0)`, which
/// collapses both bundles onto the first input's latency. Failed, as expected
/// (output 1 reported 0 instead of 10).
#[test]
fn join_reports_the_earliest_latency_in_each_bundle() {
    let sources = frame(&[
        Signal::Latency(0.0),
        Signal::Latency(10.0),
        Signal::Latency(20.0),
        Signal::Latency(30.0),
    ]);

    let bus = Routing::Join.route(&sources, 2);
    assert_eq!(bus.len(), 2);
    assert_eq!(
        latency_of(bus.at(0)),
        0.0,
        "bundle {{0, 2}} responds as soon as its earliest source does"
    );
    assert_eq!(
        latency_of(bus.at(1)),
        10.0,
        "bundle {{1, 3}} responds at 10, not at the frame-wide minimum of 0"
    );
}

/// `Routing::Reverse` mirrors the channel order, and requires the input and
/// output counts to match.
///
/// A mirror is its own inverse, which is the cheap way to catch an off-by-one:
/// `input.len() - 1 - i` and a hypothetical `input.len() - i` differ by one
/// slot, and the second panics only on the first channel — so a wrong index
/// that happened to stay in bounds would swap the wrong pair and be audible
/// only as a channel-order bug in a surround bus.
///
/// Mutation run: `input.at(input.len() - 1 - i)` → `input.at(i)`. Failed, as
/// expected.
#[test]
fn reverse_mirrors_the_channel_order() {
    let source = frame(&[
        Signal::Value(1.0),
        Signal::Value(2.0),
        Signal::Value(3.0),
        Signal::Latency(4.0),
    ]);

    let reversed = Routing::Reverse.route(&source, 4);
    assert_eq!(latency_of(reversed.at(0)), 4.0);
    assert_eq!(value_of(reversed.at(1)), 3.0);
    assert_eq!(value_of(reversed.at(2)), 2.0);
    assert_eq!(value_of(reversed.at(3)), 1.0);

    // Applying it twice is the identity.
    let back = Routing::Reverse.route(&reversed, 4);
    assert_eq!(value_of(back.at(0)), 1.0);
    assert_eq!(latency_of(back.at(3)), 4.0);
}

/// `Routing::Reverse` asserts when the output count differs from the input
/// count, rather than routing some partial mirror.
///
/// Its doc comment states the requirement ("Equal number of inputs and
/// outputs") and this is the only variant that checks one, so the check is
/// worth a test of its own — a mirror with mismatched widths has no meaning and
/// the alternative to the assert is a silently truncated or wrongly-offset
/// frame. Note the hole this does *not* cover, which
/// `an_empty_input_frame_short_circuits_every_variant` documents: an empty input
/// frame returns before the assert is reached, so `Reverse` with zero inputs and
/// four outputs does not fire.
///
/// Mutation run: the `assert_eq!(input.len(), outputs)` line deleted, with the
/// loop body's index clamped (`input.len().saturating_sub(1 + i) %
/// input.len()`) so that removing the guard does not simply trade one panic for
/// another. The test failed — nothing panicked — as expected.
#[test]
#[should_panic(expected = "assertion")]
fn reverse_rejects_a_mismatched_output_count() {
    let source = frame(&[Signal::Value(1.0), Signal::Value(2.0)]);
    let _ = Routing::Reverse.route(&source, 3);
}

/// `Routing::Arbitrary` folds every input into one signal and writes that same
/// signal to every output, whatever the two counts are.
///
/// This is the conservative default — every oscillator, filter and feedback node
/// in the fork routes through it — so the guarantee that matters is uniformity:
/// no output may claim an earlier latency than any other, because a caller
/// taking the minimum across outputs would then compensate the whole node by
/// that earliest one. The constants case is pinned alongside because `Arbitrary`
/// means "nonlinearly", and a nonlinearity is exactly what a constant does not
/// survive: a node that folded two DC inputs into a DC output would let
/// `AudioUnit::response` claim a frequency response for a waveshaper.
///
/// Mutation run: `output.fill(combo)` → `if outputs > 0 { output.set(0, combo); }`,
/// guarded so that the zero-output assertion below is not what kills it. Failed,
/// as expected: channel 1 of the three-output case was `Unknown`.
#[test]
fn arbitrary_folds_every_input_into_every_output() {
    let sources = frame(&[
        Signal::Latency(4.0),
        Signal::Latency(4.0),
        Signal::Latency(4.0),
    ]);

    // Three inputs, one output; then three inputs, five outputs. Every output
    // in both must carry the same signal.
    for outputs in [1usize, 3, 5] {
        let routed = Routing::Arbitrary(6.0).route(&sources, outputs);
        assert_eq!(routed.len(), outputs);
        for i in 0..outputs {
            assert_eq!(
                latency_of(routed.at(i)),
                10.0,
                "with {outputs} outputs, channel {i} must carry the folded signal"
            );
        }
    }

    // Zero outputs is an empty frame, not a panic.
    assert!(Routing::Arbitrary(6.0).route(&sources, 0).is_empty());

    // Constants do not survive the fold.
    let constants = frame(&[Signal::Value(1.0), Signal::Value(2.0)]);
    let routed = Routing::Arbitrary(0.0).route(&constants, 2);
    assert_unknown(routed.at(0), "constants folded nonlinearly");
    assert_unknown(routed.at(1), "constants folded nonlinearly");
}

/// **A node's reported latency does not depend on the order its input channels
/// were wired.**
///
/// It used to. The fold was `input.at(0).distort(latency)` followed by
/// `combine_nonlinear(input.at(i), latency)` per remaining channel, and *both*
/// of those add the node's own `latency` — so the extra went in once per
/// input, and the `min` inside `combine_nonlinear` then compared an
/// already-inflated running total against a raw input latency. With sources at
/// 0 and 10 and a node latency of 5, `[0, 10]` gave `min(0 + 5, 10) + 5 = 10`
/// and `[10, 0]` gave `min(10 + 5, 0) + 5 = 5`.
///
/// `route` now folds with zero extra latency and adds the node's own once at
/// the end, so both orders give `min(0, 10) + 5 = 5`. That this test was
/// originally written as a characterization of the inequality — and had to be
/// rewritten rather than updated — is why its old form asserted `a != b`.
///
/// The live case is `fundsp-tutti`'s `resynth.rs`, which routes
/// `Routing::Arbitrary(self.window_length as f64)`; every other in-tree caller
/// passes `0.0`, for which the old bug was inert because `0` is an identity
/// for the repeated addition.
///
/// *Mutation:* restore the old arithmetic — `input.at(0).distort(*latency)`
/// and `combine_nonlinear(input.at(i), *latency)`, dropping the trailing
/// `.distort(*latency)`. The two orders diverge again (10 vs 5) and the
/// equality below fails.
#[test]
fn arbitrary_latency_does_not_depend_on_channel_order() {
    const EXTRA: f64 = 5.0;
    let early_first = frame(&[Signal::Latency(0.0), Signal::Latency(10.0)]);
    let late_first = frame(&[Signal::Latency(10.0), Signal::Latency(0.0)]);

    let a = latency_of(Routing::Arbitrary(EXTRA).route(&early_first, 1).at(0));
    let b = latency_of(Routing::Arbitrary(EXTRA).route(&late_first, 1).at(0));

    assert_eq!(a, b, "channel order must not change a node's latency");
    assert_eq!(
        a,
        0.0f64.min(10.0) + EXTRA,
        "the answer is the earliest input latency plus the node's own, added once"
    );

    // Three inputs, to show the node's latency is not accumulating per channel.
    let three = frame(&[
        Signal::Latency(4.0),
        Signal::Latency(9.0),
        Signal::Latency(2.0),
    ]);
    assert_eq!(
        latency_of(Routing::Arbitrary(EXTRA).route(&three, 1).at(0)),
        2.0 + EXTRA,
        "adding EXTRA once per input would give {} here",
        2.0 + EXTRA * 3.0
    );
}

/// `Routing::Generator` ignores its input signals entirely and writes its own
/// latency to every output — **provided it has at least one input channel.**
/// See `an_empty_input_frame_short_circuits_every_variant` for the case where it
/// does not.
///
/// Ignoring the input is the whole contract: a generator's output does not
/// depend on what is fed to it, so a source with a 512-sample fill latency must
/// report 512 regardless of what its unused input ports carry. If the inputs
/// leaked through, a generator wired downstream of a lookahead limiter would
/// inherit that limiter's latency and the graph would double-compensate.
///
/// Mutation run: `output.set(i, Signal::Latency(*latency))` → `output.set(i,
/// input.at(i % input.len()))`. Failed, as expected.
#[test]
fn generator_ignores_its_inputs_and_fills_every_output_with_its_latency() {
    let noise = frame(&[Signal::Latency(512.0), Signal::Value(3.0)]);

    for outputs in [1usize, 2, 5] {
        let routed = Routing::Generator(9.0).route(&noise, outputs);
        assert_eq!(routed.len(), outputs);
        for i in 0..outputs {
            assert_eq!(
                latency_of(routed.at(i)),
                9.0,
                "with {outputs} outputs, channel {i} must report the generator's own latency"
            );
        }
    }

    assert!(Routing::Generator(9.0).route(&noise, 0).is_empty());
}

/// **An empty input frame answers `Unknown` for the four variants that read
/// their input — and `Generator`, which does not, answers its latency.**
///
/// `route` used to return early on `input.is_empty()` *before* looking at
/// `self`, which made the `Generator` arm unreachable for exactly the nodes it
/// exists for: a generator has no inputs, so the frame `AudioUnit::latency`
/// builds for it is empty. `noise`, `envelope`, `wave`, `sequencer`, `shared`
/// and `ring` all declare `Routing::Generator(0.0)` with `inputs() == 0`, and
/// every one of them was getting `Unknown` on every output.
///
/// It survived because all of them pass `0.0`: `latency()` skips `Unknown`
/// when taking its minimum and returns `None`, and the only in-tree consumer
/// (`tutti-export`) does `unwrap_or(0.0)` — so the wrong answer and the right
/// one were the same number. The first generator to declare a non-zero latency
/// would have had it silently dropped. `Generator` is now handled ahead of the
/// guard, and this test uses a **non-zero** latency precisely so that
/// coincidence cannot hide a regression.
///
/// `Reverse` keeps the guard, and that is a decision rather than an oversight:
/// an empty frame means "no signal information available", which is what a
/// frame of `Unknown` says. Making its `assert_eq!(input.len(), outputs)` fire
/// there would put a new panic on the graph-commit path to report something
/// that is not a miswiring. The assert still guards the case it can speak to —
/// see `reverse_rejects_a_mismatched_output_count`.
///
/// *Mutation:* move the `Generator` arm back below the `input.is_empty()`
/// guard. The `Generator` row then reports `Unknown` and the last assertion
/// fails.
#[test]
fn an_empty_input_frame_short_circuits_every_variant_but_the_generator() {
    const LATENCY: f64 = 9.0;
    let empty = SignalFrame::new(0);
    assert!(empty.is_empty());

    // The four that read their input. `Reverse` is here deliberately: see the
    // note above on why its assert does not fire for an empty frame.
    for (label, routing) in [
        ("Arbitrary", Routing::Arbitrary(LATENCY)),
        ("Split", Routing::Split),
        ("Join", Routing::Join),
        ("Reverse", Routing::Reverse),
    ] {
        let routed = routing.route(&empty, 4);
        assert_eq!(
            routed.len(),
            4,
            "{label}: the output width is still honoured"
        );
        for i in 0..4 {
            assert_unknown(
                routed.at(i),
                &format!("{label} with no inputs, channel {i}"),
            );
        }
    }

    // `Generator` ignores its input by definition, so an empty frame is its
    // normal case rather than a degenerate one.
    let routed = Routing::Generator(LATENCY).route(&empty, 4);
    assert_eq!(routed.len(), 4);
    for i in 0..4 {
        assert_eq!(
            latency_of(routed.at(i)),
            LATENCY,
            "a generator must report its own latency on channel {i}, not Unknown"
        );
    }
}

/// **`Routing::Join` answers a degenerate shape instead of panicking.**
///
/// Two shapes used to panic, and both were reachable from signal analysis
/// rather than from audio processing — `route` is called by
/// `AudioUnit::latency` and `AudioUnit::response`, which a host may call on an
/// arbitrary graph:
///
/// - **More outputs than inputs.** `bundle = input.len() / outputs` is integer
///   division, so it was `0`, the inner loop never ran, and the outer loop
///   reached `input.at(i)` past the end of the frame. A mono input joined to
///   two outputs indexed channel 1 of a one-channel frame.
/// - **Zero outputs from a non-empty input.** The same expression divided by
///   zero. Every other variant returns an empty frame for that shape, so
///   `Join` was alone in turning "sum these channels into nothing" into a
///   crash.
///
/// Neither had fired, because both in-tree callers (`audionode.rs`'s two join
/// nodes) have type-level arities that make the shapes unconstructible.
///
/// The answers now: zero outputs gives an empty frame, matching every other
/// variant; outputs with no input to sum keep the `Unknown` they were built
/// with, which is the honest answer rather than a wrapped duplicate of some
/// other channel's signal. `Split` wraps because it is *distributing* one
/// input across many outputs; `Join` is summing, and there is nothing to sum.
///
/// *Mutations, both run:* remove the `if outputs == 0 { return output; }`
/// guard (divide by zero), and remove the `if i >= input.len() { break; }`
/// guard (index out of bounds). Each fails its half below.
#[test]
fn join_answers_degenerate_shapes_rather_than_panicking() {
    // More outputs than inputs: channel 0 gets the input, channel 1 has
    // nothing to sum.
    let mono = frame(&[Signal::Value(1.0)]);
    let widened = Routing::Join.route(&mono, 2);
    assert_eq!(widened.len(), 2);
    assert!(
        matches!(widened.at(0), Signal::Value(_)),
        "channel 0 has an input to carry, got {}",
        describe(widened.at(0))
    );
    assert_unknown(
        widened.at(1),
        "channel 1 of a mono-to-stereo join has no input to sum",
    );

    // Zero outputs from a non-empty input: an empty frame, like every other
    // variant gives.
    let stereo = frame(&[Signal::Value(1.0), Signal::Value(2.0)]);
    let nothing = Routing::Join.route(&stereo, 0);
    assert_eq!(
        nothing.len(),
        0,
        "joining into no outputs is an empty frame, not a panic"
    );

    // And the ordinary shape still works, so the guards did not eat it.
    let quad = frame(&[
        Signal::Latency(1.0),
        Signal::Latency(2.0),
        Signal::Latency(3.0),
        Signal::Latency(4.0),
    ]);
    let joined = Routing::Join.route(&quad, 2);
    assert_eq!(joined.len(), 2);
    assert_eq!(
        latency_of(joined.at(0)),
        1.0,
        "bundle {{0, 2}} takes the earlier latency"
    );
    assert_eq!(
        latency_of(joined.at(1)),
        2.0,
        "bundle {{1, 3}} takes the earlier latency"
    );
}
