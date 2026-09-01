//! Host transport callbacks and tail time, against the AUs that ship with macOS.
//!
//! Two capabilities are covered here, both of which a DAW needs and neither of
//! which the host had:
//!
//! * **`kAudioUnitProperty_HostCallbacks`** — the four C procs an AU calls, on
//!   its render thread, to pull project tempo / beat / transport state. Without
//!   them every tempo-synced AU runs free: note-value delays, LFO sync,
//!   arpeggiators and step sequencers never lock to the project.
//! * **`kAudioUnitProperty_TailTime`** — how long an AU keeps sounding after its
//!   input goes silent. Without it an offline bounce truncates reverb and delay
//!   tails at the last note.
//!
//! ## Which AU actually calls the callbacks (and the limitation that follows)
//!
//! Measured empirically on this machine (macOS 15.6, ~35 Apple AUs + 3
//! third-party): **not one Apple Audio Unit calls any of the four procs**,
//! though every one of them *accepts* the property. The only installed unit that
//! calls them is **TAL-NoiseMaker** (`TOGU`/`ncut`), which calls
//! `beatAndTempoProc`, `musicalTimeLocationProc` and `transportStateProc`
//! exactly once per render block — and calls **v1 only**, never v2.
//!
//! TAL-NoiseMaker is not part of the corpus: it is a third-party plugin that is
//! not guaranteed to be installed, and [`corpus`](support::corpus)'s rule is
//! that a corpus member's absence is a hard failure. So the end-to-end
//! "a real AU pulled our transport" assertion lives in
//! [`a_real_au_pulls_and_consumes_the_transport`], which is `#[ignore]`d and
//! skips cleanly when the plugin is absent — the one place in this suite where
//! a skip is honest, because the subject genuinely may not exist.
//!
//! What is asserted unconditionally, against the Apple corpus, is everything
//! that does not require a plugin to call back:
//!
//! * the property is accepted by effects, instruments and mixers alike
//! * the `HostCallbackInfo` layout matches Apple's declaration byte for byte
//! * installing does not disturb rendering, and is idempotent
//! * the state plumbing — every field, the locate flag's consume-once
//!   semantics, and the render-thread read path — is driven directly, by calling
//!   the `extern "C"` procs the way AudioToolbox would
//!
//! That last group is where the real risk lives anyway: the procs are the code
//! that runs on the audio thread, and a wrong field offset or a latched locate
//! flag is a bug whether or not an Apple unit exercises it.
//!
//! ## Running
//!
//! ```bash
//! cargo test -p tutti-au-host --test au_transport
//! cargo test -p tutti-au-host --test au_transport -- --include-ignored
//! ```

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::sync::Mutex;

mod support;
use support::corpus::{
    render, silence, DELAY, DYNAMICS, EFFECTS, INSTRUMENTS, MATRIX_REVERB, NO_TAIL_EFFECTS,
    TAILLESS_UNITS, TAIL_EFFECTS, TAIL_UNSUPPORTED,
};

use tutti_au_host::types::{K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS, K_AUDIO_UNIT_SCOPE_GLOBAL};
use tutti_au_host::{AuError, TransportInfo, TransportState};
use tutti_types::meter::{BarNumber, TimeSignature};
use tutti_types::Samples;

/// Serializes component discovery / instantiate / dispose, exactly as
/// `au_conformance.rs`'s lock of the same name does. AudioToolbox tolerates
/// concurrent use of distinct units, but discovery walks a process-global
/// registry and several tests here open the same unit.
static AU_LOCK: Mutex<()> = Mutex::new(());

/// Poison recovery, for the reason `au_conformance.rs` documents: the guard is
/// only a serializer, so one panicking test must not convert into N spurious
/// failures.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    AU_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

const RATE: f64 = 48_000.0;
const BLOCK: u32 = 512;

// ------------------------------------------------------------------ tail time

/// Every effect that reports a tail must report the measured value.
///
/// Exact values, not merely "non-zero" — see [`TAIL_EFFECTS`] for why: the
/// failure mode is reading a neighbouring `Float64` global property (Latency is
/// id 12, TailTime is 20), and a latency read also returns a plausible non-zero
/// float. The pinned pairs disagree with the latency column wherever it matters.
#[test]
fn effects_report_their_measured_tail_time() {
    let _g = lock();
    for (unit, expected) in TAIL_EFFECTS {
        let au = unit.open(RATE, BLOCK);
        let tail = au
            .get_tail_time()
            .unwrap_or_else(|e| panic!("{}: tail time read failed: {e:?}", unit.label));

        assert!(
            tail.0.is_finite(),
            "{}: tail must be finite, got {}",
            unit.label,
            tail.0
        );
        // Relative tolerance: these are f64 seconds narrowed to f32, so the
        // small values (0.0046) cannot be compared with the same absolute
        // epsilon as the large ones (10.0).
        let tolerance = (expected * 1e-4).max(1e-6);
        assert!(
            (tail.0 - expected).abs() <= tolerance,
            "{}: expected tail {expected}s (measured on macOS 15.6), got {}s",
            unit.label,
            tail.0
        );
    }
}

/// A reverb's tail must be a large positive span, and must not be its latency.
///
/// The blunt version of the assertion above, kept separate because it is the one
/// that speaks to the actual user-visible bug: AUMatrixReverb reports **10
/// seconds** of tail and **zero** latency. A host that conflated the two would
/// read 0 here and truncate ten seconds of reverb off the end of every bounce.
#[test]
fn a_reverb_tail_is_large_and_distinct_from_its_latency() {
    let _g = lock();
    let au = MATRIX_REVERB.open(RATE, BLOCK);

    let tail = au.get_tail_time().expect("AUMatrixReverb reports a tail");
    let latency = au.get_latency().expect("latency read");

    assert!(
        tail.0 > 1.0,
        "AUMatrixReverb should report a multi-second tail, got {}s",
        tail.0
    );
    assert_eq!(
        latency,
        Samples::ZERO,
        "AUMatrixReverb reports no latency; if this changed, the \
         tail-vs-latency contrast below needs re-measuring"
    );
    // The two properties are genuinely different numbers, which is the whole
    // point of reading tail separately.
    assert_ne!(
        tail.to_samples_ceil(RATE),
        latency,
        "tail and latency must not be the same read"
    );
}

/// A compressor has both: a real latency *and* a real, different tail.
///
/// AUDynamicsProcessor is the one corpus unit where both properties are non-zero
/// and unequal (0.2 s tail vs 256 samples = 0.00533 s latency), so it is the
/// unit that can catch a host that reads one property and reports it as the
/// other. That swap would pass every assertion resting on a unit where one of
/// the two is zero.
#[test]
fn a_unit_with_both_latency_and_tail_reports_them_separately() {
    let _g = lock();
    let au = DYNAMICS.open(RATE, BLOCK);

    let tail = au
        .get_tail_time()
        .expect("AUDynamicsProcessor reports a tail");
    let latency = au.get_latency().expect("latency read");

    assert_eq!(
        latency,
        Samples(256),
        "AUDynamicsProcessor's 256-sample lookahead is what makes this test's \
         tail-vs-latency contrast meaningful; re-measure if it moved"
    );
    let tail_samples = tail.to_samples_ceil(RATE);
    assert_eq!(
        tail_samples,
        Samples(9601),
        "0.2s at 48kHz, rounded up: the *allocation* rounding, because a bounce \
         that rounds a tail down truncates it"
    );
    assert!(
        tail_samples > latency,
        "tail ({tail_samples:?}) and latency ({latency:?}) are different quantities \
         and must not be the same read"
    );
}

/// A unit with no tail reports exactly zero — and that is not the same fact as
/// a unit that refuses the property.
///
/// This pair is the reason [`AuInstance::get_tail_time`] propagates the refusal
/// instead of absorbing it into `Seconds(0.0)`. AUSampleDelay genuinely has no
/// tail; AUSampler cannot say. Flattening the second into the first would tell a
/// bounce that a unit of *unknown* tail has *none*, and it would truncate
/// exactly the material the property was read to protect.
#[test]
fn zero_tail_and_absent_tail_are_different_answers() {
    let _g = lock();

    for unit in NO_TAIL_EFFECTS {
        let au = unit.open(RATE, BLOCK);
        let tail = au
            .get_tail_time()
            .unwrap_or_else(|e| panic!("{}: expected a 0.0 tail, not an error: {e:?}", unit.label));
        // Assert finite as well as small: a NaN would sail through a bare
        // `< epsilon` comparison in the wrong direction on some paths, and
        // asserting "is small" without "is finite" is how a NaN-blind check
        // passes vacuously.
        assert!(
            tail.0.is_finite(),
            "{}: tail must be finite, got {}",
            unit.label,
            tail.0
        );
        assert_eq!(
            tail.0, 0.0,
            "{}: measured to report exactly zero tail",
            unit.label
        );
    }

    for unit in TAILLESS_UNITS {
        let au = unit.open(RATE, BLOCK);
        let err = au.get_tail_time().expect_err(&format!(
            "{}: every Apple instrument/mixer rejects TailTime; absorbing that \
             into Seconds(0) would make it indistinguishable from AUSampleDelay's \
             genuine zero",
            unit.label
        ));
        assert!(
            matches!(
                err,
                AuError::OsStatus {
                    code: TAIL_UNSUPPORTED,
                    ..
                }
            ),
            "{}: expected kAudioUnitErr_InvalidProperty ({TAIL_UNSUPPORTED}), got {err:?}",
            unit.label
        );
    }
}

/// Tail time must be readable before `AudioUnitInitialize`.
///
/// A host plans its bounce length — and therefore needs the tail — while wiring
/// the graph up, which is before it initializes anything. Tail is a global-scope
/// property with no render resources behind it, so gating it on the Ready state
/// would break that planning for no reason.
#[test]
fn tail_time_is_readable_before_initialize() {
    let _g = lock();
    let au = MATRIX_REVERB.open_uninitialized(RATE, BLOCK);
    assert!(!au.is_initialized());

    let tail = au
        .get_tail_time()
        .expect("tail time must be readable in the Loaded state");
    assert!(tail.0 > 1.0, "got {}s", tail.0);
}

// ----------------------------------------------------------- host callbacks

/// The property must be accepted across every unit class.
///
/// Measured: all ~35 Apple units accept the write, including the many that never
/// call back. So this asserts the host's struct is *acceptable* to AudioToolbox
/// — a wrong `size_of` or a malformed struct is rejected here with
/// `kAudioUnitErr_InvalidPropertyValue`, which is the failure this catches.
#[test]
fn host_callbacks_are_accepted_by_effects_and_instruments() {
    let _g = lock();
    for unit in EFFECTS.iter().chain(INSTRUMENTS.iter()) {
        let mut au = unit.open(RATE, BLOCK);
        au.install_host_callbacks()
            .unwrap_or_else(|e| panic!("{}: HostCallbacks rejected: {e:?}", unit.label));
        assert!(
            au.transport().is_some(),
            "{}: install must publish a TransportState",
            unit.label
        );
    }
}

/// Installing transport must not disturb rendering, and must be idempotent.
///
/// A re-install has to reuse the same boxed state rather than allocating a fresh
/// one: the AU may already hold a `hostUserData` pointer into it, and swapping in
/// a new allocation would leave that pointer dangling until the property write
/// landed — a window on the render thread. The tempo written before the second
/// install is what proves the state survived it.
#[test]
fn reinstalling_host_callbacks_preserves_the_published_state() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);

    au.install_host_callbacks().expect("first install");
    let info = TransportInfo::new().with_tempo(137.5).with_playing(true);
    assert!(au.set_transport(&info, true), "publish after install");

    au.install_host_callbacks().expect("second install");
    let state = au.transport().expect("state survives a re-install");
    assert_eq!(
        state.tempo().0,
        137.5,
        "a re-install must reuse the existing state, not replace it"
    );
    assert!(state.is_playing());

    // And the AU still renders.
    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render with transport installed");
}

/// Publishing without installing must report failure rather than silently
/// dropping the transport.
///
/// A host that forgot `install_host_callbacks` would otherwise publish into
/// nothing every block and never find out; the plugin would run free and the
/// symptom would be "tempo sync doesn't work", diagnosed nowhere near the cause.
#[test]
fn publishing_without_installing_is_reported() {
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    assert!(au.transport().is_none());
    assert!(
        !au.set_transport(&TransportInfo::new().with_tempo(120.0), false),
        "set_transport must report that no callbacks are installed"
    );
}

/// Installing before `initialize` must work, and must survive the transition.
///
/// The boxed state's address is handed to the AU as `hostUserData`, and
/// `initialize`/`uninitialize` `mem::replace` the enclosing state machine. If the
/// box body moved on those transitions the AU would be left dereferencing a
/// stale address on its render thread. Driving a full
/// install → initialize → render → uninitialize → initialize cycle is what
/// exercises that.
#[test]
fn transport_survives_initialize_and_uninitialize() {
    let _g = lock();
    let mut au = DELAY.open_uninitialized(RATE, BLOCK);

    au.install_host_callbacks()
        .expect("install in the Loaded state");
    let info = TransportInfo::new().with_tempo(90.0).with_playing(true);
    au.set_transport(&info, true);

    au.initialize()
        .expect("initialize with transport installed");
    assert_eq!(
        au.transport().expect("state survives initialize").tempo().0,
        90.0
    );

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    render(&mut au, &input, &mut output, BLOCK).expect("render");

    au.uninitialize().expect("uninitialize");
    assert_eq!(
        au.transport()
            .expect("state survives uninitialize")
            .tempo()
            .0,
        90.0
    );

    au.initialize().expect("re-initialize");
    render(&mut au, &input, &mut output, BLOCK).expect("render after the cycle");
    assert_eq!(au.transport().unwrap().tempo().0, 90.0);
}

// ------------------------------------------- the state plumbing, driven direct

/// Every `TransportInfo` field must land where the AU will read it.
///
/// Driven through the `extern "C"` procs rather than through the getters,
/// because the procs are what AudioToolbox calls and the getters do not cover
/// the time signature, cycle points, measure downbeat or sample position at all.
/// A field written into the wrong atomic would still round-trip through a getter
/// while reaching the AU as the wrong quantity.
#[test]
fn every_transport_field_reaches_the_callbacks() {
    let state = TransportState::new();
    let info = TransportInfo::new()
        .with_tempo(137.5)
        .with_playing(true)
        .with_recording(true)
        .with_position_beats(12.25, 5.0)
        .with_position_samples(240_000)
        .with_loop(true, 4.0, 20.0)
        .with_bar(8.0, BarNumber::new(3))
        .with_time_signature(TimeSignature::from_parts(7, 8));
    state.set_transport(&info, true);
    state.set_samples_to_next_beat(123);

    let procs = TestProcs::install(&state);

    // beatAndTempoProc
    let (beat, tempo) = procs.beat_and_tempo();
    assert_eq!(tempo, 137.5, "tempo");
    assert_eq!(beat, 12.25, "beat position");

    // musicalTimeLocationProc
    let (delta, num, den, downbeat) = procs.musical_time();
    assert_eq!(delta, 123, "samples to next beat");
    // The signature as *notated* — 7/8, not the 3.5-quarter bar length. Handing
    // the AU the quarter-note length would make every plugin display and every
    // bar-relative arpeggiator disagree with the host's ruler.
    assert_eq!(num, 7.0, "time signature numerator");
    assert_eq!(den, 8, "time signature denominator");
    assert_eq!(downbeat, 8.0, "current measure downbeat");

    // transportStateProc (v1)
    let v1 = procs.transport_v1();
    assert!(v1.playing, "playing");
    assert!(v1.cycling, "cycling");
    assert_eq!(v1.sample_time, 240_000.0, "sample position in timeline");
    assert_eq!(v1.cycle_start, 4.0, "cycle start beat");
    assert_eq!(v1.cycle_end, 20.0, "cycle end beat");
}

/// v1 and v2 must agree on every field they share, and v2 must add recording.
///
/// They are separate function pointers with separate bodies (v2 inserts
/// `outIsRecording` in second position, so they cannot share a signature), which
/// is exactly the shape that drifts. A host whose v1 answered differently from
/// its v2 would hand two plugins two different views of the same transport.
#[test]
fn the_two_transport_procs_agree() {
    let state = TransportState::new();
    let info = TransportInfo::new()
        .with_tempo(100.0)
        .with_playing(true)
        .with_recording(true)
        .with_position_samples(48_000)
        .with_loop(true, 2.0, 6.0);
    state.set_transport(&info, false);

    let procs = TestProcs::install(&state);
    let v1 = procs.transport_v1();
    let v2 = procs.transport_v2();

    assert_eq!(v1.playing, v2.playing, "playing");
    assert_eq!(v1.cycling, v2.cycling, "cycling");
    assert_eq!(v1.sample_time, v2.sample_time, "sample position");
    assert_eq!(v1.cycle_start, v2.cycle_start, "cycle start");
    assert_eq!(v1.cycle_end, v2.cycle_end, "cycle end");
    // The one field only v2 carries.
    assert!(v2.recording, "v2 must report recording");
}

/// The locate flag must be consumed exactly once.
///
/// Apple documents `outTransportStateChanged` as "changed since the callback was
/// last called". A host that left it latched would tell a plugin to flush its
/// internal sequencer on *every* block after the first locate — so an
/// arpeggiator would restart continuously instead of once. A host that never set
/// it would leave the plugin counting on from the old position after the user
/// jumps the playhead, which is the bug the flag exists to prevent.
#[test]
fn the_locate_flag_is_consumed_exactly_once() {
    let state = TransportState::new();
    let procs = TestProcs::install(&state);

    // No locate yet.
    assert!(!procs.transport_v1().changed, "no locate published yet");

    state.set_transport(&TransportInfo::new().with_playing(true), true);
    assert!(state.state_changed_pending(), "peek must not consume");

    assert!(
        procs.transport_v1().changed,
        "the first read after a locate must report it"
    );
    assert!(
        !procs.transport_v1().changed,
        "the flag must not latch: a second read reports no change"
    );
    assert!(!state.state_changed_pending());

    // A publish with `changed: false` must not resurrect it.
    state.set_transport(&TransportInfo::new().with_playing(true), false);
    assert!(
        !procs.transport_v1().changed,
        "a routine publish must not look like a locate"
    );

    // And v2 consumes the same flag, so a locate is not delivered twice.
    state.set_transport(&TransportInfo::new(), true);
    assert!(procs.transport_v2().changed, "v2 reports the locate");
    assert!(
        !procs.transport_v1().changed,
        "v1 must not re-report a locate v2 already consumed"
    );
}

/// A null out-param must be skipped, not written through.
///
/// Every out-param in `HostCallbackInfo`'s procs is documented nullable — the AU
/// passes null for anything it does not care about, and TAL-NoiseMaker does. A
/// blind write is a null dereference on the render thread.
#[test]
fn callbacks_tolerate_null_out_params() {
    let state = TransportState::new();
    state.set_transport(&TransportInfo::new().with_tempo(120.0), true);
    let procs = TestProcs::install(&state);

    // Every proc, with every out-param null. Reaching the asserts at all is the
    // null deref not happening.
    unsafe {
        assert_eq!(
            (procs.beat_and_tempo_fn)(procs.user_data, std::ptr::null_mut(), std::ptr::null_mut()),
            0
        );
        assert_eq!(
            (procs.musical_time_fn)(
                procs.user_data,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            0
        );
        assert_eq!(
            (procs.transport_v1_fn)(
                procs.user_data,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            0
        );
        assert_eq!(
            (procs.transport_v2_fn)(
                procs.user_data,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            0
        );
    }

    // A null out-param for the locate flag must NOT consume it: the AU never saw
    // it, so swallowing it would drop the locate on the block the plugin most
    // needed it.
    assert!(
        state.state_changed_pending(),
        "a proc that was passed a null changed-pointer must not consume the locate"
    );
}

/// A null `hostUserData` must be refused rather than dereferenced.
///
/// This is the shape the withdrawal path leaves behind: `AuLoaded::drop` clears
/// the property with an all-null struct, and an AU that races that clear can call
/// a proc with a null `hostUserData`.
#[test]
fn callbacks_refuse_a_null_user_data() {
    let state = TransportState::new();
    let procs = TestProcs::install(&state);
    let mut beat = 0.0f64;
    let mut tempo = 0.0f64;

    // kAudioUnitErr_CannotDoInCurrentContext — the status Apple's header names
    // for "the host cannot provide this right now", so a plugin that checks it
    // falls back to its own defaults instead of consuming unwritten values.
    let status = unsafe { (procs.beat_and_tempo_fn)(std::ptr::null_mut(), &mut beat, &mut tempo) };
    assert_eq!(
        status, -10863,
        "expected kAudioUnitErr_CannotDoInCurrentContext"
    );
    assert_eq!(beat, 0.0, "nothing may be written when there is no state");
    assert_eq!(tempo, 0.0);
}

/// The `HostCallbackInfo` layout must match Apple's declaration.
///
/// AudioToolbox reads this struct by offset, so a reordered or differently-sized
/// field would have the AU call whatever function pointer landed at the offset it
/// wanted — a jump to a wrong-signature function, not a clean failure. Five
/// pointer-width members, in the order `AudioUnitProperties.h` declares them.
#[test]
fn host_callback_info_matches_apples_layout() {
    // Asserted against the property's *accepted* size rather than only against
    // `size_of`, so this is Apple's opinion and not merely our own arithmetic
    // restated: the write in `install_host_callbacks` sends
    // `size_of::<HostCallbackInfo>()` bytes, and AudioToolbox rejects a
    // mismatched width with kAudioUnitErr_InvalidPropertyValue.
    let _g = lock();
    let mut au = DELAY.open(RATE, BLOCK);
    au.install_host_callbacks()
        .expect("a correctly-sized HostCallbackInfo is accepted");

    // Five pointer-sized members: hostUserData + four procs.
    let expected = 5 * std::mem::size_of::<*const c_void>();
    let mut size: u32 = 0;
    let mut writable: u8 = 0;
    let status = unsafe {
        coreaudio_sys::AudioUnitGetPropertyInfo(
            au.raw_unit(),
            K_AUDIO_UNIT_PROPERTY_HOST_CALLBACKS,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &mut size,
            &mut writable,
        )
    };
    assert_eq!(status, 0, "HostCallbacks property info");
    assert_eq!(
        size as usize, expected,
        "AudioToolbox expects a {expected}-byte HostCallbackInfo"
    );
    assert_ne!(writable, 0, "HostCallbacks must be writable");
}

// ---------------------------------------------------- the real-plugin end-to-end

/// A real AU pulls the transport and consumes the locate flag.
///
/// The only end-to-end proof available on this machine, and the reason it is
/// `#[ignore]`d: its subject is **TAL-NoiseMaker** (`TOGU`/`ncut`), a
/// third-party plugin, and no Apple unit can stand in — not one of the ~35
/// installed calls any host callback.
///
/// Measured behaviour, macOS 15.6: TAL-NoiseMaker calls `beatAndTempoProc`,
/// `musicalTimeLocationProc` and `transportStateProc` exactly once per render
/// block. It calls **v1 only** — with only `transportStateProc2` populated it
/// calls neither and receives no transport at all, which is precisely why
/// `TransportState::callback_info` fills both.
///
/// The assertion is that the locate flag, set before the render, is *cleared* by
/// the render: only the AU calling a transport proc can clear it, so a false
/// reading here is direct evidence the plugin consumed our transport.
#[test]
#[ignore = "requires the third-party TAL-NoiseMaker AU; no Apple unit calls host callbacks"]
fn a_real_au_pulls_and_consumes_the_transport() {
    use tutti_au_host::component::{enumerate_components_of_type, AuType};
    use tutti_au_host::AuInstance;

    let _g = lock();
    let Some(info) = enumerate_components_of_type(AuType::Instrument)
        .into_iter()
        .find(|c| c.sub_type == u32::from_be_bytes(*b"ncut"))
    else {
        // An honest skip: unlike a corpus member, this plugin is genuinely
        // optional. Every invariant that can be checked without it is asserted
        // unconditionally by the tests above.
        eprintln!(
            "TAL-NoiseMaker (TOGU/ncut) not installed; the end-to-end \
             transport-pull leg is not exercised"
        );
        return;
    };

    // SAFETY: `component` came from `AudioComponentFindNext` via the
    // enumeration, so it is a live factory handle for the process lifetime.
    let mut au = unsafe { AuInstance::new(info.component, RATE, BLOCK) }.expect("instantiate");
    au.install_host_callbacks().expect("install");
    au.initialize().expect("initialize");

    let transport = TransportInfo::new()
        .with_tempo(137.5)
        .with_playing(true)
        .with_position_beats(12.25, 5.0);
    au.set_transport(&transport, true);
    assert!(
        au.transport().unwrap().state_changed_pending(),
        "the locate must be pending before the AU has rendered"
    );

    let input = silence(2, BLOCK as usize);
    let mut output = silence(2, BLOCK as usize);
    for _ in 0..4 {
        render(&mut au, &input, &mut output, BLOCK).expect("render");
    }

    assert!(
        !au.transport().unwrap().state_changed_pending(),
        "TAL-NoiseMaker calls transportStateProc once per block, so the locate \
         flag must have been consumed by the render. Still pending means the AU \
         never pulled our transport — the callbacks are not reaching it."
    );
}

// ---------------------------------------------------------------- test harness

/// The four procs, recovered from a real [`TransportState`] and callable the way
/// AudioToolbox calls them.
///
/// Goes through [`TransportState::callback_info`] rather than re-declaring the
/// function pointers, so these tests exercise the *shipping* procs and the
/// *shipping* `hostUserData` wiring. A hand-rolled copy would keep passing after
/// the real ones broke.
struct TestProcs {
    user_data: *mut c_void,
    beat_and_tempo_fn: unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> i32,
    musical_time_fn:
        unsafe extern "C" fn(*mut c_void, *mut u32, *mut f32, *mut u32, *mut f64) -> i32,
    transport_v1_fn: unsafe extern "C" fn(
        *mut c_void,
        *mut u8,
        *mut u8,
        *mut f64,
        *mut u8,
        *mut f64,
        *mut f64,
    ) -> i32,
    transport_v2_fn: unsafe extern "C" fn(
        *mut c_void,
        *mut u8,
        *mut u8,
        *mut u8,
        *mut f64,
        *mut u8,
        *mut f64,
        *mut f64,
    ) -> i32,
}

/// The six fields both transport procs report.
struct TransportReadback {
    playing: bool,
    recording: bool,
    changed: bool,
    sample_time: f64,
    cycling: bool,
    cycle_start: f64,
    cycle_end: f64,
}

impl TestProcs {
    /// Pull the installed procs out of `state`'s callback info.
    fn install(state: &TransportState) -> Self {
        let info = state.test_callback_info();
        Self {
            user_data: info.0,
            beat_and_tempo_fn: info.1,
            musical_time_fn: info.2,
            transport_v1_fn: info.3,
            transport_v2_fn: info.4,
        }
    }

    fn beat_and_tempo(&self) -> (f64, f64) {
        let mut beat = f64::NAN;
        let mut tempo = f64::NAN;
        let status = unsafe { (self.beat_and_tempo_fn)(self.user_data, &mut beat, &mut tempo) };
        assert_eq!(status, 0, "beatAndTempoProc");
        (beat, tempo)
    }

    fn musical_time(&self) -> (u32, f32, u32, f64) {
        let mut delta = u32::MAX;
        let mut num = f32::NAN;
        let mut den = u32::MAX;
        let mut downbeat = f64::NAN;
        let status = unsafe {
            (self.musical_time_fn)(
                self.user_data,
                &mut delta,
                &mut num,
                &mut den,
                &mut downbeat,
            )
        };
        assert_eq!(status, 0, "musicalTimeLocationProc");
        (delta, num, den, downbeat)
    }

    fn transport_v1(&self) -> TransportReadback {
        let mut playing = 0u8;
        let mut changed = 0u8;
        let mut sample_time = f64::NAN;
        let mut cycling = 0u8;
        let mut cycle_start = f64::NAN;
        let mut cycle_end = f64::NAN;
        let status = unsafe {
            (self.transport_v1_fn)(
                self.user_data,
                &mut playing,
                &mut changed,
                &mut sample_time,
                &mut cycling,
                &mut cycle_start,
                &mut cycle_end,
            )
        };
        assert_eq!(status, 0, "transportStateProc");
        TransportReadback {
            playing: playing != 0,
            // v1 has no recording field; `false` is the absence, not a reading.
            recording: false,
            changed: changed != 0,
            sample_time,
            cycling: cycling != 0,
            cycle_start,
            cycle_end,
        }
    }

    fn transport_v2(&self) -> TransportReadback {
        let mut playing = 0u8;
        let mut recording = 0u8;
        let mut changed = 0u8;
        let mut sample_time = f64::NAN;
        let mut cycling = 0u8;
        let mut cycle_start = f64::NAN;
        let mut cycle_end = f64::NAN;
        let status = unsafe {
            (self.transport_v2_fn)(
                self.user_data,
                &mut playing,
                &mut recording,
                &mut changed,
                &mut sample_time,
                &mut cycling,
                &mut cycle_start,
                &mut cycle_end,
            )
        };
        assert_eq!(status, 0, "transportStateProc2");
        TransportReadback {
            playing: playing != 0,
            recording: recording != 0,
            changed: changed != 0,
            sample_time,
            cycling: cycling != 0,
            cycle_start,
            cycle_end,
        }
    }
}
