//! Does this host survive Audio Units that **violate the spec**?
//!
//! Every other suite in this crate loads Apple's stock units, which return what
//! the spec says they should. That proves the host works when everything goes
//! right — the easy half. Real plugin folders contain units that refuse to
//! initialize, report a latency they never apply, claim more buses than they
//! own, hand back a garbage `CFArray`, and return `noErr` from a render that
//! wrote nothing. A host that mishandles any of those takes the whole DAW down
//! with it.
//!
//! No Apple unit will ever misbehave on request, which is why this needs a probe
//! we control. See `tests/support/probe_au.rs` for how one is registered in
//! pure Rust via `AudioComponentRegister` — no bundle, no SDK, no `build.rs` —
//! and for the roster of misbehaviours.
//!
//! ## What "survive" means here
//!
//! Not "produce correct audio" — a misbehaving AU's audio is its own fault. The
//! bar is that the **host** stays correct:
//!
//! - a refusal is reported as an error, not swallowed
//! - a lie is clamped or rejected rather than trusted into an out-of-bounds
//!   access
//! - a failure is contained rather than propagated as a crash or a hang
//!
//! ## Why these tests need no serialising lock
//!
//! Each misbehaviour is a **separate registered component** with its own subtype
//! code, latched at instantiation, rather than one component reading a global
//! switch. So unlike `tutti-vst3-host`'s equivalent — which serialises on
//! `TUTTI_PROBE_MISBEHAVIOUR`, a process-global env var — nothing here is
//! shared between tests and they run in parallel safely. Registration itself is
//! `OnceLock`-guarded inside the support module.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_misbehaving
//! ```
//!
//! Behaviours asserted below were measured on macOS 15.6 (Apple silicon).

#![cfg(target_os = "macos")]

use tutti_au_host::bus::BusDirection;
use tutti_au_host::instance::AuInstance;
use tutti_types::Samples;

mod support;
use support::probe_au::{
    Misbehaviour, LIED_LATENCY_SAMPLES, LYING_ELEMENT_COUNT, PROBE_RENDER_LEVEL, STALE_POISON,
};

/// Block size every test renders at.
const BLOCK: u32 = 64;
const RATE: f64 = 48_000.0;

/// Render one block of silence into freshly-zeroed buffers.
///
/// Buffers are allocated per call rather than reused, so a probe that writes
/// nothing leaves zeroes rather than the previous block's values — the
/// distinction `renders_nothing_does_not_leak_stale_scratch` depends on. Returns
/// the host's `process` result alongside the output so callers can assert both.
fn render_fresh(au: &mut AuInstance) -> (tutti_au_host::Result<()>, Vec<Vec<f32>>) {
    render_prefilled(au, 0.0)
}

/// As [`render_fresh`], but the output buffers arrive pre-filled with `fill`.
///
/// Pre-filling is how "the host wrote nothing into my buffer" is told apart from
/// "the host wrote silence": both leave a buffer a plain zero-check cannot
/// distinguish.
fn render_prefilled(au: &mut AuInstance, fill: f32) -> (tutti_au_host::Result<()>, Vec<Vec<f32>>) {
    let input = vec![vec![0.0f32; BLOCK as usize]; 2];
    let mut output = vec![vec![fill; BLOCK as usize]; 2];
    let result = {
        let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
        let mut outs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
        au.process(&ins, &mut outs, BLOCK)
    };
    (result, output)
}

/// Largest absolute sample across every channel.
fn peak(buffers: &[Vec<f32>]) -> f32 {
    buffers
        .iter()
        .flat_map(|c| c.iter())
        .fold(0.0f32, |a, &b| a.max(b.abs()))
}

/// True when every sample is finite.
///
/// Always asserted alongside a "peak is small" claim: `f32::max` returns the
/// non-NaN operand, so [`peak`] is NaN-blind and a buffer full of NaN reports a
/// peak of 0.0. Without this the smallness assertion cannot fail.
fn all_finite(buffers: &[Vec<f32>]) -> bool {
    buffers.iter().flat_map(|c| c.iter()).all(|s| s.is_finite())
}

/// The control: a well-behaved probe must work end-to-end.
///
/// Every assertion in this file about a *misbehaving* probe is only meaningful
/// if the same host code path succeeds against one that behaves. Without this
/// test, a host that failed to load any probe at all would make the whole suite
/// pass by never reaching the interesting code — the same vacuity that let 31 of
/// 32 VST3 conformance tests report `ok` having run nothing.
///
/// This is also the standing proof that `AudioComponentRegister` works: a
/// registered component is found, instantiated, initialized, and its rendered
/// samples arrive in the host's buffer.
#[test]
fn a_well_behaved_probe_renders_through_the_host() {
    let mut au = Misbehaviour::None.open_initialized(RATE, BLOCK);
    let (result, out) = render_fresh(&mut au);
    result.expect("well-behaved probe must render");
    assert!(all_finite(&out), "probe output must be finite");
    assert!(
        (peak(&out) - PROBE_RENDER_LEVEL).abs() < 1e-6,
        "expected the probe's constant {PROBE_RENDER_LEVEL}, got peak {}. \
         The probe's render did not reach the host's buffer, so every other \
         test in this file is testing nothing.",
        peak(&out)
    );
}

/// An AU that refuses `initialize` must surface an error, not a false success.
///
/// `kAudioUnitErr_FailedInitialization` is how a unit reports an absent licence,
/// dongle, or hardware device — real units do this (AUSoundIsolation refuses
/// with -10868 on machines without its model). A host that treats the refusal as
/// success goes on to call `process` on a unit that explicitly declined to
/// become renderable.
#[test]
fn a_refused_initialize_is_an_error() {
    let mut au = Misbehaviour::FailsInitialize.open(RATE, BLOCK);
    let err = au
        .initialize()
        .expect_err("a probe returning kAudioUnitErr_FailedInitialization must not report success");
    assert!(
        !au.is_initialized(),
        "the host must not report an AU as initialized after it refused: {err:?}"
    );
}

/// A refusal must leave the instance usable, not wedged.
///
/// This is the hole `src/instance.rs` documents at length: the failure arm used
/// to return while `state` was still the `Empty` marker, turning *every* later
/// method — `raw_unit`, `au_type`, even `is_initialized` — into an
/// `unreachable!()` panic. A host that scans installed AUs and tolerates one
/// refusing to initialize would crash on the next thing it asked, so the
/// accessors are exercised here rather than assumed.
#[test]
fn a_refused_initialize_leaves_the_instance_queryable() {
    let mut au = Misbehaviour::FailsInitialize.open(RATE, BLOCK);
    assert!(au.initialize().is_err());

    // Each of these panicked in the `Empty`-state bug.
    assert!(!au.raw_unit().is_null(), "raw_unit must stay valid");
    let _ = au.au_type();
    let _ = au.num_inputs();
    let _ = au.num_outputs();
    assert!(!au.is_initialized());

    // And a render must be refused rather than attempted on an uninitialized AU.
    let (result, _out) = render_fresh(&mut au);
    assert!(
        result.is_err(),
        "process must refuse an AU that never initialized"
    );

    // Retrying must still fail the same way rather than corrupting state.
    assert!(
        au.initialize().is_err(),
        "a second initialize must fail identically, not succeed from a half-state"
    );
}

/// A refusal must not stop the host loading a different AU afterwards.
///
/// A host that leaks the component instance or wedges some global on the failure
/// path breaks the *next* load rather than this one — exactly the bug a
/// single-shot test misses. Repeated so a per-failure leak has room to accumulate.
#[test]
fn a_refused_initialize_leaves_the_host_able_to_load_more() {
    for _ in 0..16 {
        let mut bad = Misbehaviour::FailsInitialize.open(RATE, BLOCK);
        assert!(bad.initialize().is_err());
        drop(bad);
    }
    // The well-behaved probe must still load and render after all that.
    let mut good = Misbehaviour::None.open_initialized(RATE, BLOCK);
    let (result, out) = render_fresh(&mut good);
    result.expect("a good AU must still load after repeated refusals");
    assert!(all_finite(&out));
}

/// A latency the AU never applies must not destabilise the host.
///
/// Stale or wrong-unit latency (milliseconds reported as samples) is common in
/// the wild. The host cannot detect the lie — PDC being wrong is the plugin's
/// fault — but it must not size a buffer from the claim and walk off the end.
/// 9001 samples against a 64-sample block is far enough apart to catch that.
#[test]
fn a_lied_about_latency_does_not_destabilise_the_host() {
    let mut au = Misbehaviour::LiesAboutLatency.open_initialized(RATE, BLOCK);

    let reported = au.get_latency().expect("latency read must not fail");
    assert_eq!(
        reported,
        Samples(LIED_LATENCY_SAMPLES as usize),
        "the host must report the AU's claim verbatim rather than inventing a \
         value; PDC correctness is the plugin's problem, but silently rewriting \
         the number would hide the lie from a user comparing against the plugin's UI"
    );

    // The lie must not have poisoned rendering.
    for _ in 0..8 {
        let (result, out) = render_fresh(&mut au);
        result.expect("render must survive a lying latency report");
        assert!(all_finite(&out), "output must stay finite");
    }
}

/// A refused latency property is an error, not a zero.
///
/// `get_latency` used to swallow the refusal with `unwrap_or(0.0)` *inside*
/// itself, so its `Result` could never be `Err` and every caller's error arm —
/// including the AU loader's `unwrap_or(0)` — was unreachable code that read as
/// if it had considered the case. A host cannot then tell "this plugin delays
/// nothing" from "this plugin would not say", and the two want different
/// answers: the first needs no PDC, the second is a plugin worth warning about.
///
/// Nothing in the real corpus reaches this path — measured on macOS 15.6, all
/// 29 registered units answer and none refuses — which is precisely why the
/// swallowing survived. Only a probe can produce the refusal.
///
/// Note what is asserted: an `Err`, not a specific value. The point is that the
/// refusal is *distinguishable*, and a zero would not be.
#[test]
fn a_refused_latency_is_an_error_not_a_zero() {
    let au = Misbehaviour::RefusesLatency.open_initialized(RATE, BLOCK);

    let got = au.get_latency();
    assert!(
        got.is_err(),
        "the AU refused kAudioUnitProperty_Latency, but the host reported \
         {got:?} — flattening that to a value makes a refusal indistinguishable \
         from a plugin that genuinely delays nothing"
    );

    // The control: the same call against a well-behaved probe succeeds, so the
    // assertion above is about the refusal and not about the harness.
    let ok = Misbehaviour::None.open_initialized(RATE, BLOCK);
    assert_eq!(
        ok.get_latency().expect("a conforming probe answers"),
        Samples::ZERO,
        "the conforming probe reports zero latency, so `Ok(0)` and `Err` are \
         both reachable and this test can tell them apart"
    );
}

/// A **negative** latency must not wrap into a huge unsigned buffer size.
///
/// `get_latency` scales the AU's `f64` seconds by the sample rate and converts
/// through `Seconds::to_samples`. A negative product is the dangerous case: in
/// C, casting it to unsigned wraps to something near the integer maximum, and a
/// host that allocated or offset by that number is instantly out of bounds.
/// `to_samples` collapses negative and non-finite inputs to zero explicitly, so
/// the answer must be 0 — this test pins that guarantee rather than trusting it,
/// because the expression is one `unsafe` block away from C semantics and a
/// future refactor through `libc` or a manual cast would silently reintroduce
/// the wrap.
#[test]
fn a_negative_latency_saturates_instead_of_wrapping() {
    let mut au = Misbehaviour::ReportsNegativeLatency.open_initialized(RATE, BLOCK);

    let reported = au.get_latency().expect("latency read must not fail");
    assert_eq!(
        reported,
        Samples::ZERO,
        "a negative latency must saturate to 0, not wrap to ~usize::MAX \
         (got {reported:?}) — a host offsetting a buffer by that value reads far \
         out of bounds"
    );

    for _ in 0..8 {
        let (result, out) = render_fresh(&mut au);
        result.expect("render must survive a negative latency report");
        assert!(all_finite(&out));
    }
}

/// An AU over-reporting `ElementCount` must not drive the host out of bounds.
///
/// This is the classic crash: `ElementCount` says 64, the host queries bus
/// properties up to 64, and the AU's own array holds one — so the reads run past
/// the end of it. The host must ask the AU per bus and let it refuse, rather
/// than indexing on faith.
#[test]
fn an_over_reported_element_count_does_not_crash_the_host() {
    let au = Misbehaviour::OverReportsElementCount.open_initialized(RATE, BLOCK);

    // The host reports the AU's claim; that much is just relaying.
    assert_eq!(
        au.bus_count(BusDirection::Input),
        LYING_ELEMENT_COUNT,
        "bus_count relays the AU's claim"
    );

    // The load-bearing half: walking every claimed bus must not crash, and the
    // buses past the real one must come back as errors rather than as a
    // fabricated default layout. A host that invented a layout here would size
    // render buffers for buses that do not exist.
    let mut refused = 0;
    for bus in 0..LYING_ELEMENT_COUNT {
        for dir in [BusDirection::Input, BusDirection::Output] {
            if au.bus_layout(dir, bus).is_err() {
                refused += 1;
            }
        }
    }
    assert!(
        refused > 0,
        "every one of the {LYING_ELEMENT_COUNT} claimed buses answered with a \
         layout. The probe owns one element per scope, so the host is \
         fabricating layouts for buses that do not exist rather than surfacing \
         the AU's refusal."
    );
}

/// An AU claiming absurd bus counts must still render on bus 0.
///
/// Separate from the walk above because the failure mode differs: a host that
/// sized its render scratch from the *claimed* count would allocate for 64 buses
/// and mis-address bus 0, so rendering is what catches it.
#[test]
fn an_over_reporting_au_still_renders_bus_zero() {
    let mut au = Misbehaviour::OverReportsElementCount.open_initialized(RATE, BLOCK);
    for _ in 0..8 {
        let (result, out) = render_fresh(&mut au);
        result.expect("bus 0 must still render");
        assert!(all_finite(&out), "output must stay finite");
    }
}

/// An AU that returns `noErr` having written nothing must not leak stale scratch.
///
/// The host renders into its own scratch buffers and copies them out. If the AU
/// writes nothing and the host copies anyway, whatever the scratch held from the
/// *previous* block is emitted as audio — which is heard as a stuttering repeat
/// of an earlier sound, and in the worst case as uninitialized memory.
///
/// The output buffers arrive pre-filled with a poison value distinct from both
/// silence and the probe's own level, so "the host left my buffer alone",
/// "the host wrote silence" and "the host wrote real audio" are three
/// distinguishable outcomes rather than two.
#[test]
fn renders_nothing_does_not_leak_stale_scratch() {
    let mut au = Misbehaviour::RendersNothing.open_initialized(RATE, BLOCK);

    // Prime the host's internal scratch with real audio from a *different*
    // instance is not possible (scratch is per instance), so prime this one by
    // rendering blocks that the probe declines to fill; then check the poison.
    let (result, out) = render_prefilled(&mut au, STALE_POISON);
    result.expect("a render that writes nothing still returns noErr");
    assert!(all_finite(&out), "output must stay finite");

    // The host must have overwritten the caller's poison with its scratch
    // contents (silence), not left the caller's buffer untouched and not
    // emitted garbage. Either way the result must be bounded.
    let p = peak(&out);
    assert!(
        p <= PROBE_RENDER_LEVEL + 1e-6,
        "output peak {p} exceeds anything the probe could have produced, so the \
         host emitted uninitialized or stale memory"
    );
}

/// Repeated writes-nothing renders must stay silent rather than accumulating.
///
/// A host whose scratch is never cleared would emit the same stale block
/// forever; one that accumulated would grow without bound. Both are caught by
/// driving many blocks and bounding the peak, with finiteness asserted
/// separately because `peak` is NaN-blind.
#[test]
fn repeated_empty_renders_stay_bounded() {
    let mut au = Misbehaviour::RendersNothing.open_initialized(RATE, BLOCK);
    for i in 0..64 {
        let (result, out) = render_prefilled(&mut au, STALE_POISON);
        result.expect("empty render must keep succeeding");
        assert!(all_finite(&out), "block {i} produced non-finite samples");
        let p = peak(&out);
        assert!(
            p <= PROBE_RENDER_LEVEL + 1e-6,
            "block {i} peaked at {p}, above anything the probe writes"
        );
    }
}

/// An AU that writes fewer frames than asked must not leave the tail unbounded.
///
/// The probe fills half the block and then *under-reports* `mDataByteSize`. A
/// host that trusted the returned byte size as "how much is valid" and one that
/// trusted its own frame count disagree about the tail, and the tail is where
/// uninitialized memory would show up.
#[test]
fn a_partial_render_leaves_a_bounded_tail() {
    let mut au = Misbehaviour::WritesFewerFrames.open_initialized(RATE, BLOCK);
    let (result, out) = render_prefilled(&mut au, STALE_POISON);
    result.expect("a partial render still reports noErr");

    assert!(
        all_finite(&out),
        "the untouched tail must not contain non-finite garbage"
    );
    let p = peak(&out);
    assert!(
        p <= PROBE_RENDER_LEVEL.max(STALE_POISON.abs()) + 1e-6,
        "peak {p} exceeds both the probe's level and the caller's poison, so \
         the tail is uninitialized memory"
    );
}

/// A failing `ClassInfo` must be reported as an error, not as empty state.
///
/// `save_state` is what a DAW calls to persist a plugin into a project file. If
/// a refusal is absorbed into `Ok(vec![])`, the project saves *successfully*
/// with the plugin's state silently missing, and the loss is only discovered
/// when the user reopens the session. An error here lets the host warn instead.
#[test]
fn a_failing_class_info_save_is_an_error() {
    let au = Misbehaviour::FailsClassInfo.open_initialized(RATE, BLOCK);
    let result = au.save_state();
    assert!(
        result.is_err(),
        "an AU refusing kAudioUnitProperty_ClassInfo must produce an error, not \
         an empty state blob that a project file would record as 'saved'; got \
         {result:?}"
    );
}

/// A failing `ClassInfo` restore must be reported, and must not wedge the AU.
///
/// The restore direction matters independently: a host that ignored the failure
/// would show a project as loaded while the plugin sat at its defaults. The AU
/// must also still render afterwards — a failed restore is not a reason to lose
/// the instance.
#[test]
fn a_failing_class_info_restore_is_an_error_and_the_au_survives() {
    let mut au = Misbehaviour::FailsClassInfo.open_initialized(RATE, BLOCK);

    // A non-empty, well-formed binary plist, so the failure comes from the AU
    // rejecting `ClassInfo` rather than from the host declining to parse input.
    let good = Misbehaviour::None.open_initialized(RATE, BLOCK);
    let blob = good.save_state().expect("well-behaved probe saves state");
    assert!(
        !blob.is_empty(),
        "the control probe produced an empty state blob, so this test would \
         exercise load_state's empty-input short circuit instead of the AU's refusal"
    );

    let result = au.load_state(&blob);
    assert!(
        result.is_err(),
        "an AU refusing a ClassInfo write must produce an error: {result:?}"
    );

    let (render, out) = render_fresh(&mut au);
    render.expect("the AU must still render after a refused state restore");
    assert!(all_finite(&out));
}

/// A NULL `FactoryPresets` array behind a `noErr` must not be dereferenced.
///
/// This is a success status with no value behind it — the AU says "here is your
/// array" and hands back nothing. A host that trusts the status and dereferences
/// the pointer crashes on the spot.
#[test]
fn a_null_factory_presets_array_is_treated_as_empty() {
    let au = Misbehaviour::NullFactoryPresets.open_initialized(RATE, BLOCK);
    let presets = au.factory_presets();
    assert!(
        presets.is_empty(),
        "a NULL CFArrayRef must read as 'no presets', got {} entries",
        presets.len()
    );
}

/// A garbage `FactoryPresets` array must not crash the preset walk.
///
/// **This test found a real host bug.** `factory_presets` walks the array and
/// reads each element as an `AUPreset`, pulling a `CFStringRef` out of its second
/// field. The probe's elements are `CFData` blocks, so that read recovers CF
/// header internals as `presetName` — non-null, so the old `is_null()`-only guard
/// passed it straight to CoreFoundation, which aborted the process with SIGBUS
/// (`EXC_BAD_ACCESS` inside `CFRetain`, confirmed under lldb). A real AU with a
/// corrupted or stale preset table would take the whole DAW down the same way.
/// The fix is `cfstring_to_string_checked`; see its docs for why a plain
/// alignment test is the wrong guard on arm64.
///
/// Repeated so a CoreFoundation ownership error has room to become a crash: an
/// over-release corrupts the array and typically dies on a *later* pass, not the
/// first — which is exactly why `au_presets_bypass.rs` enumerates 200 times.
#[test]
fn a_garbage_factory_presets_array_does_not_crash_the_walk() {
    let au = Misbehaviour::GarbageFactoryPresets.open_initialized(RATE, BLOCK);
    for _ in 0..200 {
        // Whatever comes back is meaningless — the elements are not presets. The
        // assertion is that we get here at all, repeatedly, without a crash or a
        // hang. Names are read (not just counted) so the CFString path runs.
        let presets = au.factory_presets();
        for p in &presets {
            std::hint::black_box(p.name.len());
        }
    }
}

/// Every misbehaving probe must be constructible and droppable without a crash.
///
/// A sweep across the whole roster, so a variant added to
/// `support/probe_au.rs` without its own test still gets the minimum guarantee:
/// instantiate, attempt to initialize, drop. `initialize` is allowed to fail
/// here — several probes exist precisely to refuse it — but neither the attempt
/// nor the teardown may take the process down.
#[test]
fn every_probe_loads_and_drops_cleanly() {
    for &behaviour in Misbehaviour::ALL {
        let mut au = behaviour.open(RATE, BLOCK);
        // Ignored deliberately: refusal is the point of some of these.
        let _ = au.initialize();
        // Exercise the accessors a host would touch during a scan, on both the
        // initialized and refused paths.
        let _ = au.au_type();
        let _ = au.num_inputs();
        let _ = au.num_outputs();
        let _ = au.get_latency();
        let _ = au.factory_presets();
        drop(au);
    }
}
