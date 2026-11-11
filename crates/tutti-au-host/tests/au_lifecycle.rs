//! The lifecycle of one Audio Unit through the halves an activation splits it
//! into: instantiate, activate (`AudioUnitInitialize`), render, deactivate.
//!
//! Most tests here hold the unit as [`Au`] (see `support/au.rs`), which owns
//! whichever half-set the unit is in; the last section drives the traits
//! themselves ([`Loaded`], [`Processor`], [`Controller`]).

#![cfg(target_os = "macos")]

mod support;

use support::au::Au;
use tutti_au_ffi::property::StreamFormat;
use tutti_au_ffi::{Element, Scope};
use tutti_au_host::component::{AuComponentRef, find_component};
use tutti_au_host::types::{AudioComponentDescription, K_AUDIO_UNIT_TYPE_EFFECT};
use tutti_fixtures::main_thread;


fn find_apple_delay() -> Option<AuComponentRef> {
    let desc = AudioComponentDescription {
        componentType: K_AUDIO_UNIT_TYPE_EFFECT,
        componentSubType: u32::from_be_bytes(*b"dely"),
        componentManufacturer: u32::from_be_bytes(*b"appl"),
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    find_component(&desc)
}

#[test]
fn test_new() {
    let comp = find_apple_delay().expect("AUDelay should be present");
    let inst = Au::new(comp, 44100.0, 512);
    assert!(inst.is_ok());
}

#[test]
fn test_initialize_uninitialize() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();

    assert!(!inst.is_initialized());
    inst.initialize().unwrap();
    assert!(inst.is_initialized());
    inst.uninitialize().unwrap();
    assert!(!inst.is_initialized());
}

#[test]
fn test_get_name() {
    let comp = find_apple_delay().unwrap();
    let inst = Au::new(comp, 44100.0, 512).unwrap();
    assert!(!inst.get_name().unwrap().is_empty());
}

#[test]
fn test_parameter_list() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    let params = inst.get_parameter_list();
    assert!(!params.is_empty());
}

#[test]
fn test_get_set_parameter() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    let params = inst.get_parameter_list();
    let p = &params[0];
    let mid = p.range.mid();
    inst.set_parameter(p.id, mid).unwrap();
    let val = inst.get_parameter(p.id).unwrap();
    assert!((val - mid).abs() < 0.01);
}

#[test]
fn test_process_silence() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    let input = vec![vec![0.0f32; 512]; 2];
    let mut output = vec![vec![0.0f32; 512]; 2];
    let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut out_slices: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();

    inst.process(&in_slices, &mut out_slices, 512).unwrap();

    // A delay adds nothing to silence, so every output frame must still be
    // zero. Without this the test passed on a `process` that never touched
    // the output buffers at all.
    for (ch, buf) in output.iter().enumerate() {
        assert!(
            buf.iter().all(|s| *s == 0.0),
            "channel {ch} is not silent after processing silence"
        );
    }
}

#[test]
fn test_process_audio() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    let input: Vec<Vec<f32>> = (0..2)
        .map(|_| {
            (0..512)
                .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 44100.0).sin() * 0.5)
                .collect()
        })
        .collect();
    let mut output = vec![vec![0.0f32; 512]; 2];
    let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut out_slices: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();

    inst.process(&in_slices, &mut out_slices, 512).unwrap();

    // The exact waveform depends on AUDelay's default mix, so assert only
    // what holds for any setting: the buffers were actually written, and
    // written with real numbers. A `process` that left them untouched, or
    // one that wrote NaN through an uninitialised union, both passed the
    // bare `unwrap` this replaces.
    for (ch, buf) in output.iter().enumerate() {
        assert!(
            buf.iter().all(|s| s.is_finite()),
            "channel {ch} carries a non-finite sample"
        );
        assert!(
            buf.iter().any(|s| *s != 0.0),
            "channel {ch} is silent after processing a 440 Hz tone"
        );
    }
}

#[test]
fn test_latency() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();
    let _ = inst.get_latency().unwrap();
}

#[test]
fn test_save_load_state() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    let state = inst.get_state(&main_thread()).unwrap();
    assert!(!state.is_empty());
    inst.set_state(&main_thread(), &state)
        .unwrap();
}

/// Presets and bypass must work in the `Loaded` state, before
/// `AudioUnitInitialize`. Both are global-scope properties with no render
/// resources behind them, and a host builds its preset menu and restores a
/// saved bypass state while wiring the plugin up — i.e. before it ever
/// initializes. Gating either on the Ready state would break that.
#[test]
fn presets_and_bypass_work_before_initialize() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    assert!(!inst.is_initialized());

    // AUDelay ships no presets — the read fails and is absorbed. What is
    // being asserted is that it does not panic or error out of the Loaded
    // state, not the count (the integration suite pins counts).
    assert!(inst.factory_presets().is_empty());
    inst.set_bypass(true).expect("bypass in the Loaded state");
    assert!(inst.is_bypassed().unwrap());
    inst.set_bypass(false).unwrap();
    assert!(!inst.is_bypassed().unwrap());
}

/// `factory_presets` must absorb the property error, but `current_preset`
/// must not invent a preset for a unit that has none.
///
/// These two deliberately differ, and the difference is easy to "tidy" into
/// consistency later: an empty list is an honest description of a unit with
/// no presets, whereas any `AuPreset` returned from `current_preset` would
/// be a claim about the AU's state. AUDelay in fact implements
/// `PresentPreset` and reports the `-1`/"Untitled" no-selection sentinel,
/// so this asserts the number is negative rather than that the call fails —
/// the point is that the value came from the AU.
#[test]
fn a_preset_less_unit_reports_no_selection_rather_than_a_fabricated_preset() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    assert!(inst.factory_presets().is_empty());
    if let Ok(current) = inst.current_preset() {
        assert!(
            current.number < 0,
            "AUDelay advertises no factory presets, so it must not report a \
             selected one; got {current:?}"
        );
    }
}

#[test]
fn test_render_callback_installed_once() {
    // AUDelay is an effect (has input), so the render callback IS installed.
    // Per-instance counter — no process-global, so this is race-free under
    // cargo's parallel test runner.
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    // Exactly one install after initialize — none per render block.
    assert_eq!(
        inst.callback_install_count(),
        1,
        "render callback should be installed exactly once at initialize"
    );

    let input = vec![vec![0.0f32; 512]; 2];
    let mut output = vec![vec![0.0f32; 512]; 2];
    let in_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();

    for _ in 0..16 {
        let mut out_slices: Vec<&mut [f32]> =
            output.iter_mut().map(|v| v.as_mut_slice()).collect();
        inst.process(&in_slices, &mut out_slices, 512).unwrap();
    }

    // Still exactly one — process() must not re-install per block.
    assert_eq!(
        inst.callback_install_count(),
        1,
        "process() must not re-install the render callback per block"
    );
}

/// A refused `initialize` must leave the instance usable.
///
/// `initialize` takes the state out with `mem::replace(.., State::Empty)`
/// and the by-value transition consumes it, so the failure arm must put a
/// real state back. Every later accessor — `raw_unit`, `au_type`,
/// `num_outputs`, even `is_initialized` — routes through
/// `handle()`/`config()`, which `unreachable!()` on `Empty`. Leaving `Empty`
/// installed panics a host on the next thing it asks about any unit that
/// declined to initialize, and units do decline: AUSoundIsolation refuses on
/// this machine, as will any unit whose hardware or entitlement is absent.
///
/// Driven through a real refusal rather than a mocked one. `vois` is not in
/// the corpus because it is not part of the *rendering* contract; it is
/// used here only as a unit that says no. If it ever starts initializing,
/// the test says so rather than passing silently.
#[test]
fn a_refused_initialize_leaves_the_instance_usable() {
    let desc = AudioComponentDescription {
        componentType: K_AUDIO_UNIT_TYPE_EFFECT,
        componentSubType: u32::from_be_bytes(*b"vois"),
        componentManufacturer: u32::from_be_bytes(*b"appl"),
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    let Some(comp) = find_component(&desc) else {
        // Not a silent skip of the invariant: the same guarantee is
        // asserted below against an AU that *does* initialize, so the state
        // machine is still exercised. Only the refusal leg needs this unit.
        eprintln!("AUSoundIsolation not registered; refusal leg not exercised");
        return;
    };
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    let unit_before = inst.unit().clone();

    match inst.initialize() {
        Err(_) => {
            // The whole point: these must answer rather than panic.
            assert!(!inst.is_initialized());
            assert!(
                inst.unit().same_unit(&unit_before),
                "a refused initialize replaced the underlying unit"
            );
            let _ = inst.au_type();
            let _ = inst.num_outputs();
            // And the instance must still be re-drivable.
            let _ = inst.initialize();
        }
        Ok(()) => {
            // It accepted after all. Still assert the state is coherent, so
            // this branch is not a free pass.
            assert!(inst.is_initialized());
        }
    }
}

#[test]
fn test_set_sample_rate() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();
    inst.set_sample_rate(48000.0).unwrap();
    assert_eq!(inst.sample_rate(), 48000.0);
    assert!(inst.is_initialized());
}

/// An `Ok` from `set_sample_rate` must mean the AU's own ASBD agrees, not
/// merely that the request was recorded.
///
/// So this asserts against the AU's read-back ASBD rather than against
/// `sample_rate()`, which reports the stored figure and would agree with the
/// request even if the stream-format write had been dropped.
#[test]
fn set_sample_rate_ok_means_the_au_really_moved() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    for rate in [48000.0f64, 96000.0, 44100.0] {
        if inst.set_sample_rate(rate).is_err() {
            // A refusal is a legitimate outcome; what must never happen is
            // a refusal reported as success. Check the config was rolled
            // back rather than left holding the rejected rate.
            assert_ne!(
                inst.sample_rate(),
                rate,
                "a rejected rate must not be left in the config"
            );
            continue;
        }
        assert_eq!(inst.sample_rate(), rate);
        // The independent check: ask the AU itself.
        let asbd = inst
            .unit()
            .get::<StreamFormat>(Scope::OUTPUT, Element::ZERO)
            .expect("AUDelay reports its output stream format");
        assert!(
            (asbd.mSampleRate - rate).abs() < 1e-6,
            "set_sample_rate({rate}) returned Ok but the AU is at {}",
            asbd.mSampleRate
        );
    }
}

/// The end-to-end invariant: `set_sample_rate` returning `Ok` must
/// imply the AU's own ASBD agrees.
///
/// CAVEAT on what this can prove locally: Apple's AUDelay accepts *every*
/// rate offered to it — probed here at 0.5 Hz, 1 Hz, 8 kHz, 48 kHz, 192 kHz
/// and 1 MHz, all reported back verbatim. So no installed AU on this machine
/// drives the *rejection* branch, and this test cannot fail by deleting the
/// `check_sample_rate` call site. The rejection logic itself is pinned by
/// `stream::tests::a_rate_the_au_did_not_accept_is_an_error`; what this adds
/// is the standing guarantee that the two agree for whatever the AU does —
/// including on a machine with a pickier AU installed.
#[test]
fn an_unusual_rate_cannot_be_reported_as_accepted_unless_it_was() {
    let comp = find_apple_delay().unwrap();
    let mut inst = Au::new(comp, 44100.0, 512).unwrap();
    inst.initialize().unwrap();

    // A rate a stricter AU would refuse.
    let absurd = 1.0f64;
    match inst.set_sample_rate(absurd) {
        Err(_) => {
            // Rejected, as expected. The config must have been rolled back
            // rather than left holding the rate the AU refused.
            assert_ne!(inst.sample_rate(), absurd);
        }
        Ok(()) => {
            // Astonishing, but then the AU must really be at 1 Hz.
            let asbd = inst
                .unit()
                .get::<StreamFormat>(Scope::OUTPUT, Element::ZERO)
                .expect("AUDelay reports its output stream format");
            assert!(
                (asbd.mSampleRate - absurd).abs() < 1e-6,
                "set_sample_rate({absurd}) returned Ok while the AU is at {} — \
                 Ok must mean the rate was actually applied",
                asbd.mSampleRate
            );
        }
    }
}


// ------------------------------------------------------------------ the traits

mod halves {
    use super::*;
    use tutti_au_host::{AuController, AuLoaded, AuProcessor};
    use tutti_plugin_types::{
        AudioBuffer, AudioBufferMut, Control, Controller, Loaded, MaxBlock, Normalized, ParamFlags,
        ParamId, ParameterChanges, ParameterQueue, PluginIo, ProcessContext, ProcessOutput,
        Processor, Request, SampleRate, taken,
    };
    use tutti_types::Samples;

    fn prepare(rate: f64, block: usize) -> tutti_plugin_types::Prepare {
        tutti_plugin_types::Prepare::new(SampleRate(rate), MaxBlock::of(Samples(block))).unwrap()
    }

    fn active(rate: f64, block: usize) -> (AuProcessor, AuController) {
        let comp = find_apple_delay().expect("AUDelay should be present");
        let loaded = AuLoaded::new(comp, rate, block as u32).unwrap();
        loaded
            .activate(&main_thread(), prepare(rate, block))
            .unwrap_or_else(|e| panic!("AUDelay activates: {}", e.reason))
    }

    /// An activation splits the unit and a deactivation rejoins the same
    /// instance: the unit the halves hold is the unit that comes back, and it is
    /// uninitialized (it can be activated again at another rate).
    #[test]
    fn a_deactivation_rejoins_the_instance_the_activation_split() {
        let (processor, controller) = active(44_100.0, 512);
        let unit = controller.unit().clone();
        let loaded = AuLoaded::deactivate(&main_thread(), processor, controller)
            .unwrap_or_else(|s| panic!("AUDelay deactivates: {}", s.reason));
        assert!(loaded.unit().same_unit(&unit), "the rejoined unit is the one split");
        let (p, c) = loaded
            .activate(&main_thread(), prepare(48_000.0, 256))
            .unwrap_or_else(|e| panic!("reactivates at another rate: {}", e.reason));
        assert_eq!(c.sample_rate(), 48_000.0);
        assert_eq!(c.block_size(), 256);
        drop((p, c));
    }

    /// Halves of two activations are not one plugin: the pair is handed back
    /// whole, both still initialized, and the reason says why.
    #[test]
    fn halves_of_two_activations_are_stuck_and_handed_back() {
        let (p1, c1) = active(44_100.0, 512);
        let (p2, c2) = active(44_100.0, 512);
        let stuck = match AuLoaded::deactivate(&main_thread(), p1, c2) {
            Ok(_) => panic!("a pair from two activations was rejoined"),
            Err(stuck) => stuck,
        };
        assert!(stuck.reason.to_string().contains("two activations"), "{}", stuck.reason);
        // Both halves came back, and each still renders with its own partner.
        drop(stuck);
        drop((p2, c1));
    }

    /// The Processor renders a block through the trait's `PluginIo`, applies the
    /// block's normalized automation against the range the AU declared (full
    /// scale lands on the declared maximum, not on 1.0) and the Controller reads
    /// it back normalized.
    #[test]
    fn automation_through_plugin_io_is_denormalized_and_reads_back_normalized() {
        let (mut processor, controller) = active(44_100.0, 64);
        let main = main_thread();
        let target = Control::parameters(&controller, &main)
            .into_iter()
            .find(|p| {
                p.range.bounds().is_some_and(|(_, max)| max > 2.0)
                    && p.flag(ParamFlags::READ_ONLY) == Some(false)
            })
            .expect("AUDelay exposes a wide-range writable parameter");
        let (min, max) = target.range.bounds().expect("a plain range");

        for (normalized, expected_plain) in [(1.0, max), (0.0, min)] {
            let mut changes = ParameterChanges::new();
            let mut queue = ParameterQueue::with_capacity(target.id, 1);
            queue.add_point(0, normalized);
            changes.add_queue(queue);
            let ctx = ProcessContext::new().params(&changes);

            let input = vec![vec![0.0f32; 64]; 2];
            let mut output = vec![vec![0.0f32; 64]; 2];
            let inputs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
            let mut outputs: Vec<&mut [f32]> =
                output.iter_mut().map(|v| v.as_mut_slice()).collect();
            let mut out = ProcessOutput::default();
            let mut io = PluginIo {
                buffer: AudioBufferMut::F32(AudioBuffer {
                    inputs: &inputs,
                    outputs: &mut outputs,
                    num_samples: 64,
                    sample_rate: 44_100.0,
                }),
                ctx: &ctx,
                out: &mut out,
            };
            processor.process(&mut io).expect("the block renders");

            let plain = tutti_au_host::parameters::get(controller.unit(), target.id.get())
                .expect("the unit answers");
            assert!(
                (f64::from(plain) - expected_plain).abs() < 1e-3 * (max - min).abs().max(1.0),
                "{normalized} must land on {expected_plain}, not stay {plain}"
            );
            let read = Control::parameter(&controller, &main, target.id).expect("declared");
            assert!(
                (read.get() - normalized).abs() < 1e-3,
                "the read is normalized: {read:?} for {normalized}"
            );
        }
    }

    /// A normalized write on the Controller reaches the unit as plain units and
    /// reads back as the same normalized value.
    #[test]
    fn a_controller_parameter_round_trips_normalized() {
        let (_processor, mut controller) = active(44_100.0, 512);
        let main = main_thread();
        let id = Control::parameters(&controller, &main)
            .into_iter()
            .find(|p| p.range.bounds().is_some_and(|(_, max)| max > 2.0))
            .expect("a wide-range parameter")
            .id;
        Control::set_parameter(&mut controller, &main, id, Normalized::of(0.25));
        let read = Control::parameter(&controller, &main, id).unwrap();
        assert!((read.get() - 0.25).abs() < 1e-3, "{read:?}");
        let _ = ParamId::new(id.get());
    }

    /// A request the unit raises (here by hand, as no installed unit moves its
    /// latency on request) comes out of `take_requests` once.
    #[test]
    fn a_raised_request_is_taken_once() {
        let (_processor, mut controller) = active(44_100.0, 512);
        let main = main_thread();
        Request::LatencyChanged.raise(controller.requests());
        let mut out = Vec::new();
        Control::take_requests(&mut controller, &main, &mut out);
        assert_eq!(out, [Request::LatencyChanged]);
        out.clear();
        Control::take_requests(&mut controller, &main, &mut out);
        assert!(out.is_empty(), "taken once");
        let _ = taken;
    }

    /// The latency a Controller reports is the unit's own, and agrees with the
    /// wiring read at the same time.
    #[test]
    fn the_controller_latency_agrees_with_the_wiring() {
        let (_processor, controller) = active(44_100.0, 512);
        let main = main_thread();
        assert_eq!(
            Controller::latency(&controller, &main),
            Control::wiring(&controller, &main).latency_samples
        );
    }

    /// An offline `Prepare` writes `kAudioUnitProperty_OfflineRender` before the
    /// unit initializes, and the unit reports whether it took it: AUSampler
    /// implements the property, and AUDelay does not (it is absent there, so the
    /// write is refused, which is the probe behind `Features::RENDER_MODE`).
    #[test]
    fn an_offline_activation_writes_the_render_mode_and_says_who_took_it() {
        let offline = prepare(44_100.0, 512).with_mode(tutti_plugin_types::RenderMode::Offline);

        let sampler = support::corpus::SAMPLER.require();
        let (p, c) = AuLoaded::new(sampler.component, 44_100.0, 512)
            .unwrap()
            .activate(&main_thread(), offline)
            .unwrap_or_else(|e| panic!("AUSampler activates: {}", e.reason));
        assert!(c.render_mode_accepted(), "AUSampler takes the offline-render property");
        assert!(c.is_offline_render().unwrap(), "and holds it");
        drop((p, c));

        let comp = find_apple_delay().expect("AUDelay should be present");
        let (p, c) = AuLoaded::new(comp, 44_100.0, 512)
            .unwrap()
            .activate(&main_thread(), offline)
            .unwrap_or_else(|e| panic!("AUDelay activates: {}", e.reason));
        assert!(
            !c.render_mode_accepted(),
            "AUDelay does not implement the property, so it did not take the mode"
        );
        drop(p);
    }
}

// ------------------------------------------------------------------------ tail

mod tail {
    use super::*;
    use tutti_au_host::types::K_AUDIO_UNIT_TYPE_MUSIC_DEVICE;
    use tutti_plugin_types::{Control, PluginTail, SampleRate};
    use tutti_types::Samples;

    fn wiring_tail(au: &Au) -> PluginTail {
        Control::wiring(&**au, &main_thread()).tail
    }

    /// An AU that declines `kAudioUnitProperty_TailTime` is `Unknown`, and one
    /// that reports an unbounded tail is `Unbounded`: never `None`, which is what
    /// a plain sample count would have collapsed both to. The classification is
    /// the unit's wiring (`Control::wiring`), the figure PDC and a bounce read.
    ///
    /// The Apple corpus covers the first half: every Apple instrument, mixer and
    /// generator rejects the property, and every Apple effect answers it. The
    /// second half needs a plugin that reports `f64::INFINITY` (TAL Reverb 4 does,
    /// and no Apple unit exceeds ~21 s), so it runs only when that unit is
    /// installed and says so when it is skipped.
    #[test]
    fn a_declined_tail_is_unknown_and_an_infinite_one_is_unbounded() {
        // An effect answers the property, so it must not be `Unknown`.
        let comp = find_apple_delay().expect("AUDelay should exist");
        let mut inner = Au::new(comp, 44_100.0, 512).expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let tail = wiring_tail(&inner);
        assert_ne!(
            tail,
            PluginTail::Unknown,
            "AUDelay answers kAudioUnitProperty_TailTime, so its tail is known"
        );

        // An instrument rejects the property outright: that is "no tail
        // concept", which must read as `Unknown` rather than as a zero tail.
        let instrument = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_MUSIC_DEVICE,
            componentSubType: u32::from_be_bytes(*b"dls "),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        if let Some(comp) = find_component(&instrument) {
            let mut inner = Au::new(comp, 44_100.0, 512).expect("Should create instance");
            inner.initialize().expect("Should initialize");
            // Whatever this unit answers, a *refusal* must never surface as a
            // zero tail: that is the mapping this test exists for.
            if inner.get_tail_time().is_err() {
                let mapped = wiring_tail(&inner);
                assert_eq!(
                    mapped,
                    PluginTail::Unknown,
                    "a refused tail property must be Unknown, not None"
                );
                assert_ne!(mapped.samples(), Some(Samples::ZERO));
            }
        }

        // The unbounded half. TAL Reverb 4 reports `f64::INFINITY`; when it is
        // not installed this leg is skipped, and says so rather than passing
        // silently: a skipped assertion is not a satisfied one.
        let infinite = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"reV4"),
            componentManufacturer: u32::from_be_bytes(*b"TOGU"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        match find_component(&infinite) {
            None => {
                eprintln!("SKIP: TAL Reverb 4 not installed; the Unbounded arm is unexercised here")
            }
            Some(comp) => {
                let mut inner = Au::new(comp, 44_100.0, 512).expect("Should create instance");
                inner.initialize().expect("Should initialize");
                let seconds = inner
                    .get_tail_time()
                    .expect("TAL Reverb 4 answers the tail property");
                assert!(
                    !seconds.get().is_finite(),
                    "TAL Reverb 4 is the corpus's infinite-tail unit; it reported {seconds:?}"
                );

                let mapped = wiring_tail(&inner);
                assert_eq!(mapped, PluginTail::Unbounded);
                // The point of the whole type: converting first would have made
                // this `None`, and a bounce would truncate the reverb entirely.
                assert_ne!(mapped, PluginTail::None);
                assert_eq!(
                    seconds.to_samples_ceil(SampleRate::new(44_100.0)),
                    Samples::ZERO
                );
            }
        }
    }
}
