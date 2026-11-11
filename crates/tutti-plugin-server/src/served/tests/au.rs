//! `Served<AuLoaded>` against the units macOS ships. macOS only.

use super::*;
use tutti_au_host::AuLoaded;
use tutti_fixtures::main_thread;
use tutti_plugin_ipc::BridgeMessage;
use tutti_plugin_types::{
    Control, Event, Hand, HostOp, Lifecycle, MaxBlock, ProcessContext, ProcessOutput, Request,
    RescanScope, SampleRate, Samples,
};

type AuInstance = Served<AuLoaded>;

/// The test-side views of the instance.
impl Served<AuLoaded> {
    /// The unit, whichever half holds it.
    fn loaded_ref(&self) -> Option<&AuLoaded> {
        match &self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => Some(plugin),
            Lifecycle::Active { controller, .. } => Some(controller),
            Lifecycle::Poisoned => None,
        }
    }

    /// The word the unit's property listener raises, for a test that needs to
    /// say a change no installed unit will make on request.
    fn requests_for_test(&self) -> std::sync::Arc<tutti_plugin_types::RequestWord> {
        std::sync::Arc::clone(self.loaded_ref().expect("the unit is loaded").requests())
    }

    /// Overwrite the cached latency, so a test can prove a poll replaces it
    /// rather than leaves it.
    ///
    /// Without this the assertion "`loaded()` and the event agree" is satisfied
    /// by *any* AU whose latency does not change during the test, which is
    /// every AU: both values would read the load-time figure and match, refresh
    /// or no refresh. Poisoning the cache first is what makes the two figures
    /// differ when the refresh is missing.
    fn poison_cached_latency(&mut self, samples: Samples) {
        self.wiring.latency_samples = samples;
    }
}

/// An AU to run the property-notification tests below against.
///
/// Deliberately a *bundled* unit, not a named third-party one: nothing these
/// tests assert is specific to a particular AU: what is under test is the
/// wiring from a property flag to a re-read, so depending on an installed
/// plugin would fail every checkout that lacks it.
///
/// `AUPeakLimiter` ships with macOS, so it is present wherever these tests
/// can run at all. It also reports a non-zero latency (88 at 44.1 kHz),
/// which most bundled units do not (`AUDelay`, `AUMatrixReverb`, `AUNBandEQ`
/// and the filters all report zero). That is not what makes the latency test
/// valid (`poison_cached_latency` does), but it keeps the figures under test
/// away from a value that coincides with the default.
///
/// `AU_SAMPLE_PLUGIN` overrides the choice with a bundle path (its first
/// Audio Unit) for a machine that has something more interesting installed.
fn au_fixture() -> Option<(String, Load)> {
    fn loadable(path: &str, load: &Load) -> bool {
        AuInstance::load_with(&main_thread(), Path::new(path), load, 44100.0, 512).is_ok()
    }

    if let Ok(named) = std::env::var("AU_SAMPLE_PLUGIN") {
        if loadable(&named, &Load::new()) {
            return Some((named, Load::new()));
        }
        eprintln!("AU_SAMPLE_PLUGIN={named} would not load; falling back");
    }
    const BUNDLE: &str = "/System/Library/Components/CoreAudio.component";
    const FALLBACK: &str = "au.aufx.lmtr.appl";
    let load = Load::new().plugin(FALLBACK);
    if loadable(BUNDLE, &load) {
        return Some((BUNDLE.to_string(), load));
    }
    eprintln!("no loadable AU found (tried {FALLBACK}); skipping");
    None
}

/// An AU latency, tail or parameter-list change must reach the drained
/// event list.
///
/// Without the property watch's requests reaching the core, an AU that changes
/// its latency on a mode switch leaves PDC compensating the load-time figure
/// permanently, with the property listener carrying the signal and nobody
/// reading it.
///
/// The requests are raised directly rather than by making a plugin move its
/// latency, because nothing installed on this machine changes its latency on
/// request: what is under test here is the *wiring* from request to message.
/// The listener half (a real `kAudioUnitProperty_Latency` notification raises
/// the request) is pinned in `tutti-au-host`'s `au_property_watch.rs`, against a
/// probe that can post one.
#[test]
fn an_au_property_change_reaches_the_event_list() {
    let _lock = crate::test_utils::plugin_load_lock();
    let Some((fixture, load)) = au_fixture() else {
        return;
    };
    let mut plugin =
        AuInstance::load_with(&main_thread(), Path::new(&fixture), &load, 44100.0, 512)
            .expect("the AU fixture resolved but would not load");

    // Nothing pending: the drain must be empty, or the assertions below
    // would pass on a stuck request rather than on the ones raised here.
    assert!(
        plugin.poll(&main_thread()).is_empty(),
        "a freshly loaded plugin reported an event nobody raised"
    );

    let requests = plugin.requests_for_test();
    Request::LatencyChanged.raise(&requests);
    Request::TailChanged.raise(&requests);
    Request::RescanParams(RescanScope::Info).raise(&requests);

    let events = plugin.poll(&main_thread());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BridgeMessage::LatencyChanged { .. })),
        "the latency change was polled and dropped; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BridgeMessage::TailChanged { .. })),
        "the tail change went missing; got {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BridgeMessage::PluginParamTitlesChanged)),
        "a parameter-list change must ask the client to re-pull the list; \
         got {events:?}"
    );

    // All three are consumed, so a second drain is quiet: an event that
    // re-fired every block would spam the host with resyncs, and a latency
    // event does a PDC re-plan each time.
    assert!(
        plugin.poll(&main_thread()).is_empty(),
        "the requests were not cleared by the drain"
    );
}

/// A latency change must refresh the wiring the client reads, not only the
/// event it emits.
///
/// The two answering differently is the load-time-only read's failure in
/// permanent form: `loaded().latency_samples` would keep reporting the
/// figure captured at load while the event carried a newer one, and a client
/// that trusted the wiring over the event would compensate the stale value
/// forever.
#[test]
fn an_au_latency_poll_refreshes_the_cached_wiring() {
    let _lock = crate::test_utils::plugin_load_lock();
    let Some((fixture, load)) = au_fixture() else {
        return;
    };
    let mut au = AuInstance::load_with(&main_thread(), Path::new(&fixture), &load, 44100.0, 512)
        .expect("the AU fixture resolved but would not load");

    // Poison the cache before polling. Without this the test cannot fail:
    // no installed AU changes its latency on request, so `loaded()` and the
    // re-read both return the load-time figure and agree whether or not the
    // refresh happened.
    let real_latency = au.loaded().latency_samples;
    au.poison_cached_latency(Samples(real_latency.0 + 1234));

    let requests = au.requests_for_test();
    Request::LatencyChanged.raise(&requests);
    Request::TailChanged.raise(&requests);

    let events = au.poll(&main_thread());
    let reported_latency = events
        .iter()
        .find_map(|e| match e {
            BridgeMessage::LatencyChanged { samples } => Some(*samples),
            _ => None,
        })
        .expect("a raised latency request must produce a re-read");
    let reported_tail = events
        .iter()
        .find_map(|e| match e {
            BridgeMessage::TailChanged { tail } => Some(*tail),
            _ => None,
        })
        .expect("a raised tail request must produce a re-read");

    // The re-read must report the AU's real figure, not the poisoned one:
    // otherwise the poll echoed the cache instead of asking the unit.
    assert_eq!(
        reported_latency, real_latency,
        "the emitted latency must come from a fresh read, not the cache"
    );
    assert_eq!(
        au.loaded().latency_samples,
        reported_latency,
        "loaded() and the emitted event must not disagree about latency"
    );
    assert_eq!(
        au.loaded().tail,
        reported_tail,
        "loaded() and the emitted event must not disagree about tail"
    );
}

use tutti_plugin_types::{
    AudioBuffer as TuttiAudioBuffer, AudioBufferMut, Features, ParamFlags, ParameterChanges,
    ParameterQueue, TimeSignature, TransportInfo,
};

const RATE: f64 = 44_100.0;
/// The bundle every Apple unit is declared in.
const CORE_AUDIO: &str = "/System/Library/Components/CoreAudio.component";

fn delay() -> AuInstance {
    AuInstance::load_with(
        &main_thread(),
        Path::new(CORE_AUDIO),
        &Load::new().plugin("au.aufx.dely.appl"),
        RATE,
        512,
    )
    .expect("AUDelay loads")
}

/// One stereo block of `frames` through `au`, with the given context pieces.
fn run(
    au: &mut AuInstance,
    frames: usize,
    transport: Option<&TransportInfo>,
    changes: Option<&ParameterChanges>,
) -> PluginResult<()> {
    let input = vec![vec![0.0f32; frames]; 2];
    let mut output = vec![vec![0.0f32; frames]; 2];
    let inputs: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    let buffer = TuttiAudioBuffer {
        inputs: &inputs,
        outputs: &mut outputs,
        num_samples: frames,
        sample_rate: RATE,
    };
    let mut ctx = ProcessContext::new();
    ctx.transport = transport;
    ctx.param_changes = changes;
    let mut out = ProcessOutput::default();
    au.render(AudioBufferMut::F32(buffer), &ctx, &mut out)
}

#[test]
fn test_au_enumerate_and_load() {
    use tutti_au_host::component;

    let effects = component::enumerate_components_of_type(tutti_au_host::component::AuType::Effect);
    assert!(
        !effects.is_empty(),
        "Should find at least one AU effect on macOS"
    );

    eprintln!("Found {} AU effects", effects.len());
    for (i, info) in effects.iter().take(5).enumerate() {
        eprintln!("  [{}] {} ({})", i, info.name, info.manufacturer);
    }
}

#[test]
fn test_au_parameter_list_via_trait() {
    let au = delay();
    let params = au.parameters(&main_thread());
    assert!(
        !params.is_empty(),
        "AUDelay should have parameters via PluginInstance trait"
    );
}

/// The loader's metadata is the unit's: an effect's buses are the unit's own
/// and its editor is measured, its transport callbacks are offered (the
/// `TRANSPORT` bit), and it receives no MIDI.
#[test]
fn a_loaded_unit_reports_its_wiring_and_capabilities() {
    let au = delay();
    let loaded = au.loaded();
    assert_eq!(loaded.inputs.len(), 1, "AUDelay has one input bus");
    assert_eq!(loaded.outputs.len(), 1, "and one output bus");
    assert!(
        loaded.features.contains(Features::TRANSPORT),
        "the loader must offer transport to every unit that accepts it"
    );
    assert!(
        !loaded.features.contains(Features::MIDI_IN),
        "an effect takes no MIDI"
    );
    assert!(
        au.descriptor().id.starts_with("au."),
        "{}",
        au.descriptor().id
    );
}

#[test]
fn test_au_process_via_trait() {
    let mut au = delay();
    // No crash is the assertion for the render itself; the block must succeed.
    run(&mut au, 512, None, None).expect("a block renders");
}

/// The loader offers `kAudioUnitProperty_HostCallbacks` at load and publishes
/// each block's transport into it, so a tempo-synced AU that pulls through
/// the procs sees the block's tempo, beat, signature, distance to the next
/// beat and a locate flag that fires once per start/seek.
///
/// No Apple AU calls the procs (see `tutti-au-host`'s `au_transport` suite),
/// so the pull is made the way AudioToolbox makes it, through the procs the
/// loader installed. What this pins is the loader's wiring; that an AU
/// *behaves* on the values needs a tempo-synced unit on macOS hardware and
/// is not asserted here.
#[test]
fn the_loader_installs_host_callbacks_and_feeds_each_blocks_transport() {
    // kAudioUnitErr_CannotDoInCurrentContext.
    const CANNOT_DO: i32 = -10863;
    const FRAMES: usize = 512;

    let mut au = delay();

    // What the AU's procs answer now: (beat_and_tempo status, beat, tempo,
    // delta, numerator, denominator, v1 changed, v1 playing).
    let pull = |au: &AuInstance| {
        let state = au
            .loaded_ref()
            .and_then(|l| l.transport())
            .expect("callbacks installed at load");
        let p = state.test_pull();
        (
            p.status,
            p.beat,
            p.tempo,
            p.delta,
            p.numerator,
            p.denominator,
            p.changed,
            p.playing,
        )
    };

    // Nothing published: the host cannot say, so no fabricated 120 BPM.
    assert_eq!(pull(&au).0, CANNOT_DO, "before any block");
    run(&mut au, FRAMES, None, None).expect("process");
    assert_eq!(
        pull(&au).0,
        CANNOT_DO,
        "a block without transport publishes none"
    );

    // Playing, 137.5 BPM, 3/4, a quarter of the way past beat 2.
    let beat0 = 2.25;
    let start = TransportInfo::new()
        .with_sample_rate(RATE)
        .with_tempo(137.5)
        .with_time_signature(TimeSignature::of(3, 4))
        .with_playing(true)
        .with_position_beats(beat0, tutti_plugin_types::Seconds(0.0));
    run(&mut au, FRAMES, Some(&start), None).expect("process");
    let (status, beat, tempo, delta, num, den, changed, playing) = pull(&au);
    assert_eq!(status, 0);
    assert_eq!((beat, tempo), (beat0, 137.5));
    // 0.75 beat left at 60 / 137.5 s per beat: 14 432.7 frames at 44.1 kHz.
    assert_eq!(delta, 14_433, "frames to the next whole beat");
    assert_eq!((num, den), (3.0, 4));
    assert!(playing);
    assert!(changed, "the first block is a start");

    // The next block is where playing leaves the playhead: no locate.
    let next = start.with_position_beats(
        beat0 + FRAMES as f64 / RATE * 137.5 / 60.0,
        tutti_plugin_types::Seconds(0.0),
    );
    run(&mut au, FRAMES, Some(&next), None).expect("process");
    assert!(!pull(&au).6, "a continuous block is not a locate");

    // A seek is one.
    let seek = next.with_position_beats(32.0, tutti_plugin_types::Seconds(0.0));
    run(&mut au, FRAMES, Some(&seek), None).expect("process");
    let (_, beat, _, delta, _, _, changed, _) = pull(&au);
    assert_eq!(beat, 32.0);
    assert_eq!(delta, 0, "on a beat");
    assert!(changed, "a seek is reported once");

    // Stopped is published too, not skipped.
    run(&mut au, FRAMES, Some(&seek.with_playing(false)), None).expect("process");
    let (status, _, _, _, _, _, changed, playing) = pull(&au);
    assert_eq!(status, 0);
    assert!(!playing && changed, "a stop is a change");

    // A block with no transport takes it back, and the next one that has
    // one is a start again even where playing would have put the playhead.
    run(&mut au, FRAMES, Some(&next), None).expect("process");
    run(&mut au, FRAMES, None, None).expect("process");
    assert_eq!(pull(&au).0, CANNOT_DO, "no frozen beat after a gap");
    let resumed = next.with_position_beats(
        next.position.beats + 0.026,
        tutti_plugin_types::Seconds(0.0),
    );
    run(&mut au, FRAMES, Some(&resumed), None).expect("process");
    let (status, _, _, _, _, _, changed, _) = pull(&au);
    assert_eq!(status, 0);
    assert!(changed, "the block after a gap starts afresh");
}

/// A unit whose host callbacks were never installed (a unit that refused the
/// property is in the same state: the host keeps no transport for it) has no
/// `TRANSPORT` bit and is not published to: the Processor holds no state to
/// write into, so a block with a transport renders and publishes nothing.
///
/// The refusal itself is `tutti-au-host`'s
/// `a_refused_host_callbacks_install_is_an_error`, against the probe unit
/// that refuses it.
#[test]
fn a_unit_without_the_callbacks_is_not_published_to() {
    let loaded = AuLoaded::new(
        tutti_au_host::component::enumerate_components()
            .into_iter()
            .find(|c| c.name.contains("AUDelay"))
            .expect("AUDelay ships with macOS")
            .component,
        RATE,
        512,
    )
    .expect("AUDelay instantiates");
    assert!(loaded.transport().is_none(), "nothing installed it");
    let control: &dyn Control = &loaded;
    assert!(
        !control
            .wiring(&main_thread())
            .features
            .contains(Features::TRANSPORT),
        "no callbacks, no TRANSPORT bit"
    );

    let mut au = AuInstance {
        lifecycle: Lifecycle::new(loaded),
        state: Lifecycle::<AuLoaded>::initial_state(),
        hand: Hand::local(),
        descriptor: Default::default(),
        wiring: Default::default(),
        requests: Vec::new(),
        events: Vec::new(),
        refused: false,
    };
    let prepare = Prepare::new(SampleRate(RATE), MaxBlock::of(Samples(512))).unwrap();
    assert!(
        au.drive(&main_thread(), Event::Op(HostOp::Activate(prepare)))
            .is_none()
    );
    let transport = TransportInfo::new().with_tempo(150.0).with_playing(true);
    run(&mut au, 512, Some(&transport), None).expect("a block with a transport renders");
    assert!(au.loaded_ref().and_then(|l| l.transport()).is_none());
}

/// The live half of the direct path: `PluginParams` is normalized for every
/// format, so a write of `1.0` must reach AUDelay's cutoff as 22050 Hz and
/// read back as `1.0`, not as the 1 Hz that writing the normalized value
/// straight through would set.
#[test]
fn direct_parameter_path_is_normalized_end_to_end() {
    let mut au = delay();
    let main = main_thread();

    // The cutoff parameter is the one whose range makes the bug audible.
    let cutoff = au
        .parameters(&main)
        .into_iter()
        .find(|p| {
            p.range
                .bounds()
                .is_some_and(|(min, max)| min >= 10.0 && max >= 20_000.0)
        })
        .expect("AUDelay declares a wide-range cutoff");
    let (min, max) = cutoff.range.bounds().expect("a plain range");
    let addr = cutoff.id;

    au.set_parameter(&main, addr, Normalized::of(1.0));
    let plain = tutti_au_host::parameters::get(au.loaded_ref().unwrap().unit(), addr.get())
        .expect("cutoff is readable");
    assert!(
        (f64::from(plain) - max).abs() < 1.0,
        "normalized 1.0 must set the top of {:?}, got {plain}",
        (min, max)
    );
    assert!(
        plain > 2.0,
        "a plain {plain} means the normalized value went through unscaled: \
             the inaudible-filter bug"
    );
    assert!((au.normalized(&main, addr) - 1.0).abs() < 1e-3);

    au.set_parameter(&main, addr, Normalized::of(0.0));
    assert!((au.normalized(&main, addr)).abs() < 1e-3);
}

#[test]
fn test_au_state_roundtrip_via_trait() {
    let mut au = delay();

    let state = au.save_state(&main_thread()).expect("save should succeed");
    assert!(!state.is_empty(), "State should not be empty");

    au.load_state(&main_thread(), &state)
        .expect("restore should succeed");
}

/// Live half: drive a real AU's automation path, assert the unit holds the
/// native value, then assert the read path inverts it.
///
/// Full-scale automation must land on the parameter's declared MAXIMUM, not
/// on `1.0` (the denormalization), and `get_parameter` must answer `1.0`
/// rather than the maximum (the `PluginParams` contract, normalized for every
/// format). Both directions in one test because either alone can pass while
/// the pair is inconsistent.
#[test]
fn param_automation_denormalizes_and_reads_back_normalized() {
    let mut au = delay();

    // Pick a writable parameter with a genuinely wide plain range: one
    // whose max is far from 1.0, so identity-vs-denormalized is decidable.
    let target = au
        .parameters(&main_thread())
        .into_iter()
        .find(|p| {
            p.range.bounds().is_some_and(|(_, max)| max > 2.0)
                && p.flag(ParamFlags::READ_ONLY) == Some(false)
        })
        .expect("AUDelay should expose a wide-range writable parameter");
    let (min, max) = target
        .range
        .bounds()
        .expect("AU declares a plain range for every parameter");

    for (normalized, expected) in [
        (1.0f64, max),
        (0.0f64, min),
        (0.5f64, min + 0.5 * (max - min)),
    ] {
        let mut changes = ParameterChanges::new();
        let mut queue = ParameterQueue::with_capacity(target.id, 1);
        queue.add_point(0, normalized);
        changes.add_queue(queue);
        run(&mut au, 64, None, Some(&changes)).expect("process should succeed");

        // Tolerance scales with the range: AU stores parameters as f32, so
        // a 22 kHz range round-trips to ~1e-4 relative precision.
        let tolerance = (max - min).abs() * 1e-4;

        // The subject of this test: the automation point was normalized,
        // and the AU must hold the *native* value. Read straight off the
        // unit rather than through `get_parameter`, which normalizes: going
        // through it would assert the identity of two conversions and pass
        // even if both were wrong.
        let native = f64::from(
            tutti_au_host::parameters::get(au.loaded_ref().unwrap().unit(), target.id.get())
                .expect("param is readable"),
        );
        assert!(
            (native - expected).abs() <= tolerance,
            "param {} ('{}'): normalized {normalized} should reach the AU as \
                 {expected} in native units, got {native} (range [{min}, {max}])",
            target.id,
            target.name,
        );

        // And the read path inverts it.
        let read_back = au.normalized(&main_thread(), target.id);
        assert!(
            (read_back - normalized).abs() <= 1e-4,
            "param {} ('{}'): should read back as the normalized {normalized}, \
                 got {read_back}",
            target.id,
            target.name,
        );
    }
}

/// The display seam denormalizes before asking the AU, and reports the AU's
/// silence as `None` rather than inventing a label.
///
/// **This asserts an absence, and that is the honest measurement.** No Apple
/// AU on macOS 15.6 implements `ParameterStringFromValue` (probed in
/// `tutti-au-host`'s `au_param_display` suite, which pins the same thing one
/// layer down), so no Apple fixture can demonstrate a *string* coming back.
/// What is still worth pinning here is that the seam asks about the **right
/// value**: the write path and the text path must denormalize identically,
/// so a third-party AU answering for a `[10, 22050]` Hz cutoff does not
/// describe `1 Hz` where the caller named full scale.
#[test]
fn au_parameter_text_denormalizes_and_reports_absence_honestly() {
    let mut au = delay();
    let main = main_thread();

    let target = au
        .parameters(&main)
        .into_iter()
        .find(|p| p.range.bounds().is_some_and(|(_, max)| max > 2.0))
        .expect("AUDelay should expose a wide-range parameter");
    let (min, max) = target.range.bounds().expect("AU declares a plain range");

    // The write path lands full scale on `max`, not on the raw `1.0`.
    au.set_parameter(&main, target.id, Normalized::of(1.0));
    let after = tutti_au_host::parameters::get(au.loaded_ref().unwrap().unit(), target.id.get())
        .expect("param is readable");
    let tolerance = (max - min).abs() * 1e-4;
    assert!(
        (f64::from(after) - max).abs() <= tolerance,
        "param {} ('{}'): normalized 1.0 must denormalize to {max}, got {after}",
        target.id,
        target.name,
    );

    // The AU is asked, and declines.
    assert_eq!(
        au.parameter_text(&main, target.id, Normalized::of(1.0)),
        None,
        "param {} ('{}'): no Apple AU implements ParameterStringFromValue. \
             A Some() here means macOS gained the property: re-measure and \
             tighten this into a real round-trip rather than relaxing it.",
        target.id,
        target.name,
    );
    assert_eq!(
        au.parameter_value_from_text(&main, target.id, "1000 Hz"),
        None,
        "nor ParameterValueFromString: a fabricated number here would be \
             written into the user's preset",
    );
}
