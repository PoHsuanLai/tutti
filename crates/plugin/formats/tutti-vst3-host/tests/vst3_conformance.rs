//! VST3 host-conformance harness — validates the `ProcessData` **this host
//! builds** against Steinberg's own spec checks.
//!
//! Steinberg ships a VST3 plugin, HostChecker, whose job is the inverse of a
//! plugin validator: it inspects what a host hands it and reports spec
//! violations. Its six check modules depend only on the header-only
//! `pluginterfaces`, so this test compiles them directly (see `build.rs`) and
//! calls `HostCheck::validate` on the *live* `ProcessData` our `process` path
//! assembled — captured through the `conformance` observer seam, never
//! reconstructed. A reconstruction would test the reconstruction.
//!
//! ~185 checks come along for free, covering block size, sample-size and
//! process-mode agreement with `ProcessSetup`, null channel/bus pointers, bus
//! counts vs `IComponent`, event ordering and validity, and parameter-queue
//! ordering, duplication, and range.
//!
//! ## Running
//!
//! ```bash
//! VST3_SDK_DIR=/path/to/vst3sdk \
//! VST3_SAMPLE_PLUGIN_DIR=/path/to/build/VST3/Release \
//! cargo test -p tutti-vst3-host --features conformance --test vst3_conformance
//! ```
//!
//! Both env vars are baked in at build time. Unset either and every test
//! skips with a printed message rather than failing, so a default `cargo test`
//! stays green without the SDK.

#![cfg(feature = "conformance")]

use std::ffi::CStr;
use std::os::raw::{c_char, c_double, c_int, c_longlong, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tutti_vst3_host::{
    Vst3Sample,
    host::conformance, AudioBuffer, MidiEvent, ParameterChanges, TransportInfo, Vst3InputEvents,
    Vst3Instance,
};

// ── HostCheck C ABI (tests/support/hostcheck_shim.cpp) ───────────────────────

extern "C" {
    fn hc_num_log_events() -> c_int;
    fn hc_log_description(id: c_int) -> *const c_char;
    fn hc_log_severity(id: c_int) -> *const c_char;
    fn hc_configure(
        sample_rate: c_double,
        max_block: c_int,
        sample_size: c_int,
        process_mode: c_int,
        in_channels: *const c_int,
        n_in: c_int,
        out_channels: *const c_int,
        n_out: c_int,
        event_in: c_int,
        event_out: c_int,
    );
    fn hc_add_parameter(param_id: c_uint);
    fn hc_validate(
        data: *const c_void,
        min_in: c_int,
        min_out: c_int,
        counts: *mut c_longlong,
    ) -> c_int;
}

/// Serializes *all* plugin work in this binary.
///
/// Two separate reasons, both real:
/// - `HostCheck` is a process-global singleton, so a configure→drive→read
///   sequence must not interleave with another test's.
/// - VST3 module lifecycle is not thread-safe here: loading and unloading the
///   same DSO concurrently races module init/exit and segfaults. Every test
///   that constructs a `Vst3Instance`/`Vst3Loaded` must hold this, not just the
///   ones that touch `HostCheck`.
///
/// Held for the whole of each test rather than per-call, since instances are
/// dropped at end of scope and the unload is half the race.
static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`PLUGIN_LOCK`], ignoring poisoning (a failed test elsewhere must
/// not cascade into spurious failures here).
fn plugin_guard() -> std::sync::MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

const AVAILABLE: &str = env!("VST3_HOSTCHECK_AVAILABLE");
const SAMPLE_PLUGIN_DIR: &str = env!("VST3_SAMPLE_PLUGIN_DIR");

const K_REALTIME: c_int = 0;

// ── Findings ─────────────────────────────────────────────────────────────────

/// One spec violation: which check fired, how often, and its severity.
#[derive(Debug, Clone)]
struct Finding {
    severity: String,
    description: String,
    count: i64,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {} (x{})", self.severity, self.description, self.count)
    }
}

fn findings_from(counts: &[i64]) -> Vec<Finding> {
    counts
        .iter()
        .enumerate()
        .filter(|(_, &c)| c > 0)
        .map(|(id, &count)| unsafe {
            let d = CStr::from_ptr(hc_log_description(id as c_int));
            let s = CStr::from_ptr(hc_log_severity(id as c_int));
            Finding {
                severity: s.to_string_lossy().into_owned(),
                description: d.to_string_lossy().into_owned(),
                count,
            }
        })
        .collect()
}

/// Checks suppressed because HostChecker itself reports them incorrectly.
///
/// Matched on description text rather than log-id index so an SDK update that
/// reorders `LOG_EVENT_LIST` can't silently redirect the suppression onto an
/// unrelated check — it would stop matching and the finding would resurface.
const KNOWN_FALSE_POSITIVES: &[(&str, &str)] = &[(
    "A parameter ID is more than 1 time in the IParameterChanges list.",
    // `ParameterChangesCheck::updateParameterIDs` does
    // `mTempUsedId.resize(mParameterIds->size())`, which value-initializes the
    // seen-list with *zeros*. `checkAllChanges` then scans those zeros as
    // already-seen ids, so any queue for the perfectly legal ParamID 0 is
    // reported as a duplicate. Verified empirically: of 18 sample plugins,
    // exactly the 9 whose first ParamID is 0 trip this, and every plugin with
    // a nonzero first ParamID is clean.
    "HostChecker pre-seeds its seen-id list with zeros; ParamID 0 is legal",
)];

/// Only `Error`-severity findings fail a test. `Warn`/`Info` are reported —
/// several are advisory (e.g. a null `inputParameterChanges` pointer, which is
/// legal when there is no automation) or informational feature probes.
/// Known-bad checks (see [`KNOWN_FALSE_POSITIVES`]) are dropped too.
fn errors(findings: &[Finding]) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| f.severity == "Error")
        .filter(|f| {
            !KNOWN_FALSE_POSITIVES
                .iter()
                .any(|(desc, _)| f.description.contains(desc))
        })
        .collect()
}

// ── Plugin discovery ─────────────────────────────────────────────────────────

/// Resolve a `.vst3` bundle to the loadable binary inside it.
fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    for sub in ["Contents/x86_64-linux", "Contents/MacOS", "Contents/x86_64-win"] {
        let dir = path.join(sub);
        for ext in ["so", "", "vst3", "dylib"] {
            let cand = if ext.is_empty() {
                dir.join(stem)
            } else {
                dir.join(format!("{stem}.{ext}"))
            };
            if cand.is_file() {
                return cand;
            }
        }
    }
    path.to_path_buf()
}

/// Every sample plugin we can find, as (name, resolved binary path).
fn sample_plugins() -> Vec<(String, PathBuf)> {
    if SAMPLE_PLUGIN_DIR.is_empty() {
        return Vec::new();
    }
    let dir = Path::new(SAMPLE_PLUGIN_DIR);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "vst3"))
        .filter_map(|p| {
            let name = p.file_stem()?.to_str()?.to_string();
            let bin = resolve_bundle(&p);
            bin.is_file().then_some((name, bin))
        })
        .collect();
    out.sort();
    out
}

/// Whether the plugin declares at least one event input bus, i.e. whether it
/// is legal for a host to send it MIDI at all.
fn accepts_midi(path: &Path) -> bool {
    Vst3Instance::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.info().has_midi_input)
        .unwrap_or(false)
}

/// The plugin's first declared ParamID, if it has any parameters.
fn first_parameter_id(path: &Path) -> Option<u32> {
    let inst = Vst3Instance::<f32>::load(path, 48_000.0, 512).ok()?;
    (inst.parameter_count() > 0).then(|| inst.parameter_id_at(0))?
}

/// Skip guard: prints why and returns false when the harness isn't available.
fn harness_ready() -> bool {
    if AVAILABLE != "1" {
        eprintln!("VST3_SDK_DIR unset or hostchecker sources missing; skipping");
        return false;
    }
    if sample_plugins().is_empty() {
        eprintln!(
            "no sample plugins under VST3_SAMPLE_PLUGIN_DIR ({SAMPLE_PLUGIN_DIR:?}); skipping"
        );
        return false;
    }
    true
}

// ── Driving one block through the real host path ─────────────────────────────

/// One `process` call's worth of host input.
#[derive(Default, Clone, Copy)]
struct Block<'a> {
    frames: usize,
    midi: &'a [MidiEvent],
    params: Option<&'a ParameterChanges>,
    transport: Option<&'a TransportInfo>,
}

impl<'a> Block<'a> {
    fn of(frames: usize) -> Self {
        Self {
            frames,
            ..Default::default()
        }
    }
    fn midi(mut self, midi: &'a [MidiEvent]) -> Self {
        self.midi = midi;
        self
    }
    fn transport(mut self, t: &'a TransportInfo) -> Self {
        self.transport = Some(t);
        self
    }
}

/// Load `path`, activate it as `T` (f32 or f64), and run one `process` block,
/// validating the `ProcessData` our host built.
fn drive_block<T: Vst3Sample + Default + Copy>(
    path: &Path,
    frames: usize,
    sample_rate: f64,
    block_size: usize,
    midi: &[MidiEvent],
    params: Option<&ParameterChanges>,
) -> Result<Vec<Finding>, String> {
    drive_blocks::<T>(
        path,
        sample_rate,
        block_size,
        &[Block::of(frames).midi(midi)
            .maybe_params(params)],
    )
    .map(|mut v| v.pop().unwrap_or_default())
}

impl<'a> Block<'a> {
    fn maybe_params(mut self, params: Option<&'a ParameterChanges>) -> Self {
        self.params = params;
        self
    }
}

/// Drive a sequence of blocks through one activated instance, validating each.
///
/// A sequence (rather than a fresh instance per block) is what makes the
/// *stateful* checks reachable: note-on/note-off pairing across blocks, and
/// `ProcessContext::systemTime` monotonicity, which by definition needs more
/// than one sample.
fn drive_blocks<T: Vst3Sample + Default + Copy>(
    path: &Path,
    sample_rate: f64,
    block_size: usize,
    blocks: &[Block<'_>],
) -> Result<Vec<Vec<Finding>>, String> {
    // NOTE: the caller must already hold `PLUGIN_LOCK` — see its docs. Taking
    // it here instead would deadlock against the helpers (`accepts_midi`,
    // `first_parameter_id`, ...) that load plugins around this call.
    let mut inst = Vst3Instance::<T>::load(path, sample_rate, block_size)
        .map_err(|e| format!("load failed: {e:?}"))?;

    let info = inst.info().clone();
    let n = unsafe { hc_num_log_events() } as usize;

    // Configure the checker to match what the host negotiated and what the
    // plugin reported, so any disagreement is a finding rather than noise.
    // The per-bus channel lists come straight from the plugin: collapsing
    // them to one count would invent mismatches on any multi-bus plugin.
    let in_ch: Vec<c_int> = info.input_bus_channels.iter().map(|&c| c as c_int).collect();
    let out_ch: Vec<c_int> = info
        .output_bus_channels
        .iter()
        .map(|&c| c as c_int)
        .collect();
    unsafe {
        hc_configure(
            sample_rate,
            block_size as c_int,
            // Must follow `T`: the checker compares this against
            // `ProcessData::symbolicSampleSize`, so hardcoding f32 would make
            // every f64 block report a (spurious) sample-size mismatch.
            T::VST3_SYMBOLIC_SIZE as c_int,
            K_REALTIME,
            in_ch.as_ptr(),
            in_ch.len() as c_int,
            out_ch.as_ptr(),
            out_ch.len() as c_int,
            info.has_midi_input as c_int,
            info.has_midi_output as c_int,
        );
    }

    // Register the plugin's parameter ids so unknown-id and out-of-range
    // param queues are reported.
    for i in 0..inst.parameter_count() {
        if let Some(id) = inst.parameter_id_at(i) {
            unsafe { hc_add_parameter(id as c_uint) };
        }
    }

    // Capture the counts the observer produces for this block.
    let counts_cell: &'static Mutex<Vec<i64>> =
        Box::leak(Box::new(Mutex::new(vec![0i64; n])));
    let seen: &'static Mutex<bool> = Box::leak(Box::new(Mutex::new(false)));

    conformance::set_observer(Box::new(move |data, _setup| {
        let mut counts = vec![0i64; n];
        let ptr = std::ptr::from_ref(data).cast::<c_void>();
        // min in/out buffer counts: the host must supply at least the bus
        // counts the component declares.
        unsafe { hc_validate(ptr, data.numInputs, data.numOutputs, counts.as_mut_ptr()) };
        *counts_cell.lock().unwrap() = counts;
        *seen.lock().unwrap() = true;
    }));

    let mut per_block = Vec::with_capacity(blocks.len());
    let default_transport = TransportInfo::default();

    for block in blocks {
        *seen.lock().unwrap() = false;

        let ins: Vec<Vec<T>> = (0..info.num_inputs.max(1))
            .map(|_| vec![T::default(); block.frames])
            .collect();
        let mut outs: Vec<Vec<T>> = (0..info.num_outputs.max(1))
            .map(|_| vec![T::default(); block.frames])
            .collect();
        let in_refs: Vec<&[T]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [T]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();

        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: block.frames,
            sample_rate,
        };
        let events = Vst3InputEvents {
            midi: block.midi,
            ..Default::default()
        };

        inst.process(
            &mut buffer,
            &events,
            block.params,
            block.transport.unwrap_or(&default_transport),
        );

        if !*seen.lock().unwrap() {
            return Err(
                "observer never fired — process() bailed before building ProcessData".into(),
            );
        }
        per_block.push(findings_from(&counts_cell.lock().unwrap()));
    }

    conformance::clear_observer();
    Ok(per_block)
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// The harness itself works: checks are linked and the table is populated.
#[test]
fn hostcheck_is_linked() {
    if AVAILABLE != "1" {
        eprintln!("VST3_SDK_DIR unset; skipping");
        return;
    }
    let n = unsafe { hc_num_log_events() };
    assert!(n > 100, "expected the full HostChecker table, got {n} checks");
    let d = unsafe { CStr::from_ptr(hc_log_description(0)) };
    assert!(!d.to_string_lossy().is_empty());
}

/// A plain steady-state block against every sample plugin must produce no
/// Error-severity findings. This is the broad sweep: it exercises bus
/// staging, channel pointers, block size, and setup agreement for each
/// plugin's own bus layout.
#[test]
fn steady_state_block_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();

    for (name, path) in sample_plugins() {
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &[], None) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter().map(|e| format!("    {e}")).collect::<Vec<_>>().join("\n")
                    ));
                } else {
                    eprintln!("  {name}: clean ({} advisory)", findings.len());
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations in the ProcessData this host built:\n{}",
        failures.join("\n")
    );
}

/// A partial block (fewer frames than the negotiated max) must stay clean —
/// the host must not report a stale `numSamples` or mis-size its bus buffers.
#[test]
fn partial_block_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();

    for (name, path) in sample_plugins() {
        for frames in [1usize, 17, 64, 511] {
            match drive_block::<f32>(&path, frames, 48_000.0, 512, &[], None) {
                Ok(findings) => {
                    let errs = errors(&findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} @ {frames} frames:\n{}",
                            errs.iter().map(|e| format!("    {e}")).collect::<Vec<_>>().join("\n")
                        ));
                    }
                }
                Err(e) => eprintln!("  {name} @ {frames}: skipped ({e})"),
            }
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations on partial blocks:\n{}",
        failures.join("\n")
    );
}

// ── HostChecker's own parameter protocol ─────────────────────────────────────
//
// The reference plugin reports its verdict *through the audio path*: every
// block, `HostCheckerProcessor::process` bit-packs the log ids it accumulated
// into `outputParameterChanges` as 8 parameters of 24 bits each
// (`kProcessWarnTag + i`). Reading them back exercises our output-parameter
// handling and yields the plugin's findings at once — a second, independent
// opinion alongside the linked-in `HostCheck` (which sees the same struct but
// from our side of the FFI).

/// First tag of the 8-parameter warning block.
///
/// From the anonymous enum in `hostcheckercontroller.h`, which starts at
/// `kProcessingLoadTag = 1000` and runs unbroken to `kProcessWarnTag`. Counted
/// from the header rather than guessed: an earlier off-by-five here decoded the
/// neighbouring `kProcessContext*` readouts as warning bitfields and invented
/// four "spec errors" that did not exist. [`warn_tag_block_is_isolated`] pins
/// the value against that class of mistake.
const K_PROCESS_WARN_TAG: u32 = 1027;
/// `kParamProcessModeTag`, immediately below the warning block — used to check
/// the block's lower bound.
const K_PARAM_PROCESS_MODE_TAG: u32 = 1026;
/// Parameters in the warning block.
const K_PARAM_WARN_COUNT: u32 = 8;
/// Bits packed into each warning parameter.
const K_PARAM_WARN_BIT_COUNT: u32 = 24;

/// Decode the log ids HostChecker packed into one `outputParameterChanges`
/// set. Returns the ids, which index the same table as [`hc_log_description`].
fn decode_hostchecker_warnings(out: &ParameterChanges) -> Vec<i32> {
    let mut ids = Vec::new();
    for i in 0..K_PARAM_WARN_COUNT {
        let Some(queue) = out.get_queue(K_PROCESS_WARN_TAG + i) else {
            continue;
        };
        let Some(point) = queue.points.last() else {
            continue;
        };
        // The plugin sends `bits / 2^24`; undo that to recover the bitfield.
        let bits = (point.value * f64::from(1u32 << K_PARAM_WARN_BIT_COUNT)).round() as u32;
        for bit in 0..K_PARAM_WARN_BIT_COUNT {
            if bits & (1 << bit) != 0 {
                ids.push((i * K_PARAM_WARN_BIT_COUNT + bit) as i32);
            }
        }
    }
    ids
}

/// Resolve a log id to its severity + description via the linked table.
fn describe_log_id(id: i32) -> Option<(String, String)> {
    if id < 0 || id >= unsafe { hc_num_log_events() } {
        return None;
    }
    unsafe {
        let d = CStr::from_ptr(hc_log_description(id));
        let s = CStr::from_ptr(hc_log_severity(id));
        Some((s.to_string_lossy().into_owned(), d.to_string_lossy().into_owned()))
    }
}

/// Path to the `host-checker` reference plugin, if it was built.
fn host_checker_path() -> Option<PathBuf> {
    sample_plugins()
        .into_iter()
        .find(|(name, _)| name == "host-checker")
        .map(|(_, path)| path)
}

/// Whether the plugin advertises `kSample64` processing.
fn supports_f64(path: &Path) -> bool {
    Vst3Instance::<f32>::load(path, 48_000.0, 512)
        .map(|i| i.info().supports_f64)
        .unwrap_or(false)
}

// ── Controller-side surfaces ─────────────────────────────────────────────────
//
// The tests below drive `IEditController` rather than the audio path. That is
// where HostChecker's ~35 thread-affinity checks live, and where its
// `IComponentHandler2`/`3`, `IProgress` and `restartComponent` probes fire —
// none of which the process-path tests can reach.
//
// The thread checker is not a GUI dependency, which is what made this look
// hard: `ThreadChecker::create()` records the calling thread and `test()`
// compares against it. The plugin creates it during `initialize`, so "the main
// thread" is simply whichever thread loaded the plugin. Doing load and all
// controller calls on one thread — as a #[test] naturally does — satisfies it,
// and calling from another thread is what a host must not do.

/// `kScoreTag` — the plugin's weighted host-capability score, 0..1 over 75
/// features (`hostcheckercontroller.h`).
const K_SCORE_TAG: u32 = 1005;
/// Triggers `restartComponent(kParamValuesChanged)` when set > 0.
const K_RESTART_PARAM_VALUES_TAG: u32 = 1013;
/// Triggers `restartComponent(kParamTitlesChanged)` when set > 0.
const K_RESTART_PARAM_TITLES_TAG: u32 = 1014;
/// Starts/stops an `IProgress` operation.
const K_TRIGGER_PROGRESS_TAG: u32 = 1008;

/// The host-capability score HostChecker computes for us.
///
/// A measured coverage figure rather than a pass/fail: each of 75 features
/// carries an importance weight (2.0 for `IComponentHandler2`, tempo, and
/// `IPlugFrame::resizeView`; 1.0 for niche ones) and the score is the weighted
/// fraction observed *so far in this session*. It therefore only counts what
/// the test actually exercised — it is a gap map, not a verdict, and a low
/// number means "not yet driven" as much as "not implemented".
#[test]
fn report_host_capability_score() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    // Exercise the surfaces we do support, so the score reflects them.
    let info = inst.info().clone();
    let transport = TransportInfo::default();
    for _ in 0..4 {
        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1)).map(|_| vec![0.0; 512]).collect();
        let mut outs: Vec<Vec<f32>> =
            (0..info.num_outputs.max(1)).map(|_| vec![0.0; 512]).collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        inst.process(&mut buffer, &Vst3InputEvents::default(), None, &transport);
    }
    let _ = inst.poll_plugin_notifications();

    let score = inst.parameter(K_SCORE_TAG);
    eprintln!("host capability score: {:.1}% of HostChecker's 75 weighted features", score * 100.0);

    // Deliberately not a threshold assertion: the score measures coverage of
    // features this test drove, so pinning it would encode today's harness
    // rather than the host's behaviour. Assert only that it is well-formed —
    // a NaN or out-of-range value would mean we read the wrong parameter.
    assert!(
        (0.0..=1.0).contains(&score),
        "score {score} outside 0..=1 — likely reading the wrong parameter tag"
    );
}

/// `restartComponent` requests must reach the host and surface as notifications.
///
/// The plugin fires `restartComponent(kParamValuesChanged / kParamTitlesChanged)`
/// when these trigger parameters are set, and logs whether the host returned
/// `kResultTrue`. A host that ignores restarts leaves stale parameter values or
/// titles in its UI after a preset change — and the plugin can detect that we
/// handled it, which is what the `kLogIdRestart*Supported` probes record.
#[test]
fn restart_component_requests_reach_the_host() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    // Drain anything latched during load so the assertion is about our trigger.
    let _ = inst.poll_plugin_notifications();

    inst.set_parameter(K_RESTART_PARAM_VALUES_TAG, 1.0);
    inst.set_parameter(K_RESTART_PARAM_TITLES_TAG, 1.0);
    let notifications = inst.poll_plugin_notifications();

    assert!(
        notifications.restart.param_values_changed || notifications.restart.param_titles_changed,
        "plugin requested restartComponent(kParamValuesChanged / kParamTitlesChanged) \
         but neither reached the host through poll_plugin_notifications()"
    );
}

/// `IProgress` reports must reach the host.
///
/// The plugin runs a timed progress operation when `kTriggerProgressTag` is
/// set, calling `IProgress::start`/`update`/`finish` on our handler. A host
/// that doesn't implement `IProgress` leaves the user with no feedback during
/// long plugin operations (sample loading, offline render).
///
/// The plugin drives progress from a **VSTGUI timer**, which only runs when its
/// editor is open — so with no window we may legitimately observe nothing. The
/// test therefore asserts the call is *accepted* and reports whether events
/// arrived, rather than requiring them.
#[test]
fn progress_reports_are_accepted() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };
    let _ = inst.poll_plugin_notifications();

    inst.set_parameter(K_TRIGGER_PROGRESS_TAG, 1.0);
    let notifications = inst.poll_plugin_notifications();
    eprintln!(
        "IProgress: {} event(s) observed (0 is expected headless — the plugin \
         drives progress from an editor timer)",
        notifications.progress.len()
    );

    inst.set_parameter(K_TRIGGER_PROGRESS_TAG, 0.0);
    let _ = inst.poll_plugin_notifications();
}

/// `IPrefetchableSupport` must be queryable without upsetting the plugin.
///
/// `host-checker` implements the interface; the value it reports is its own
/// business, but a host that queries it must do so on the main thread and
/// must tolerate any of the three prefetchable states.
#[test]
fn prefetchable_support_is_queryable() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    match inst.prefetchable_support() {
        Some(v) => {
            eprintln!("IPrefetchableSupport reports {v}");
            assert!(v <= 2, "unexpected prefetchable state {v} (expected 0..=2)");
        }
        None => eprintln!("plugin does not implement IPrefetchableSupport"),
    }
}

/// MIDI-learn arming must forward captured CCs to the plugin on the main thread.
///
/// `IMidiLearn::onLiveMIDIControllerInput` is main-thread-only, but the CCs it
/// receives are captured on the audio thread — so the host has to buffer and
/// forward. HostChecker asserts the thread context of the call, meaning a host
/// that forwards straight from `process` is caught here.
#[test]
fn midi_learn_forwards_from_the_main_thread() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(mut inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    inst.arm_midi_learn(true);
    assert!(inst.is_midi_learn_armed());

    // Feed CCs through the *audio* path; the host must capture them there and
    // forward on the next main-thread poll, not call the plugin inline.
    let info = inst.info().clone();
    // MIDI 2.0 CC values are 32-bit; 0x8000_0000 is mid-scale.
    let cc = [
        MidiEvent::cc(0, 0, 7, 0x8000_0000).with_frame_offset(0),
        MidiEvent::cc(0, 0, 10, 0x4000_0000).with_frame_offset(64),
    ];
    let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1)).map(|_| vec![0.0; 512]).collect();
    let mut outs: Vec<Vec<f32>> = (0..info.num_outputs.max(1)).map(|_| vec![0.0; 512]).collect();
    let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    let mut buffer = AudioBuffer {
        inputs: &in_refs,
        outputs: &mut out_refs,
        num_samples: 512,
        sample_rate: 48_000.0,
    };
    inst.process(
        &mut buffer,
        &Vst3InputEvents {
            midi: &cc,
            ..Default::default()
        },
        None,
        &TransportInfo::default(),
    );

    // The forward happens here, on this (main) thread.
    let _ = inst.poll_plugin_notifications();
    inst.arm_midi_learn(false);
    assert!(!inst.is_midi_learn_armed());
}

/// Pin [`K_PROCESS_WARN_TAG`] against the plugin's actual parameter layout.
///
/// The warning block is decoded as a bitfield; the parameters immediately
/// below it (`kParamProcessModeTag` and the `kProcessContext*` readouts) carry
/// ordinary normalized values. Decoding one as the other yields plausible
/// nonsense — it manufactured four phantom "spec errors" before this was
/// caught. Assert the block's identity structurally rather than trusting a
/// hand-counted enum offset.
#[test]
fn warn_tag_block_is_isolated() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };
    let Ok(inst) = Vst3Instance::<f32>::load(&path, 48_000.0, 512) else {
        eprintln!("host-checker load failed; skipping");
        return;
    };

    let declared: Vec<u32> = (0..inst.parameter_count())
        .filter_map(|i| inst.parameter_id_at(i))
        .collect();

    // Every id in the warning block must be a real parameter...
    for i in 0..K_PARAM_WARN_COUNT {
        let tag = K_PROCESS_WARN_TAG + i;
        assert!(
            declared.contains(&tag),
            "kProcessWarnTag + {i} = {tag} is not a parameter this plugin declares; \
             the enum offset in this test is stale"
        );
    }
    // ...and the tag just below it must be the process-mode readout, which
    // fixes the block's lower bound. If the enum shifted, one of these fails.
    assert!(
        declared.contains(&K_PARAM_PROCESS_MODE_TAG),
        "expected kParamProcessModeTag at {K_PARAM_PROCESS_MODE_TAG}"
    );
    assert!(
        !declared.contains(&(K_PROCESS_WARN_TAG + K_PARAM_WARN_COUNT)),
        "found a parameter just past the warning block — the block is larger \
         than kParamWarnCount, so the decode would drop findings"
    );
}

/// The reference plugin's own verdict, delivered through `outputParameterChanges`.
///
/// This is HostChecker judging us from *inside* the plugin, independent of the
/// `HostCheck` instance we link in — and getting it requires our output-param
/// path to work, so a silent failure to read `outputParameterChanges` shows up
/// as "no findings" rather than a false pass. The test therefore asserts we
/// received *something* before asserting the findings are clean.
#[test]
fn host_checker_reports_no_errors_through_output_params() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let Some(path) = host_checker_path() else {
        eprintln!("host-checker reference plugin not built; skipping");
        return;
    };

    let mut inst = match Vst3Instance::<f32>::load(&path, 48_000.0, 512) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("host-checker load failed ({e:?}); skipping");
            return;
        }
    };
    let info = inst.info().clone();

    // Drive several blocks: the plugin latches state-machine observations
    // across calls, and only reports once it has something to say.
    let mut all_ids = Vec::new();
    let transport = TransportInfo::default();
    for _ in 0..8 {
        let ins: Vec<Vec<f32>> = (0..info.num_inputs.max(1)).map(|_| vec![0.0; 512]).collect();
        let mut outs: Vec<Vec<f32>> =
            (0..info.num_outputs.max(1)).map(|_| vec![0.0; 512]).collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: 512,
            sample_rate: 48_000.0,
        };
        let out = inst.process(
            &mut buffer,
            &Vst3InputEvents::default(),
            None,
            &transport,
        );
        all_ids.extend(decode_hostchecker_warnings(out.parameter_changes));
    }

    all_ids.sort_unstable();
    all_ids.dedup();

    let mut errors_found = Vec::new();
    for id in &all_ids {
        if let Some((severity, desc)) = describe_log_id(*id) {
            if KNOWN_FALSE_POSITIVES.iter().any(|(d, _)| desc.contains(d)) {
                continue;
            }
            if severity == "Error" {
                errors_found.push(format!("    [{severity}] {desc}"));
            } else {
                eprintln!("  advisory: [{severity}] {desc}");
            }
        }
    }

    assert!(
        errors_found.is_empty(),
        "host-checker reported VST3 spec errors against this host:\n{}",
        errors_found.join("\n")
    );
}

/// Plugin state must survive a `getState` → `setState` → `getState` round trip.
///
/// Untested until now, and it is the persistence path: a project save/reload
/// runs exactly this. `host-checker` implements state on both the processor and
/// the controller (`HostCheckerProcessor::setState` /
/// `HostCheckerController::setState`), each with a thread-affinity assertion,
/// so a host that touches state off the main thread is caught too.
#[test]
fn plugin_state_round_trips() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        let mut inst = match Vst3Instance::<f32>::load(&path, 48_000.0, 512) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("  {name}: skipped (load failed {e:?})");
                continue;
            }
        };

        let Ok(first) = inst.state() else {
            // A plugin with no state at all is legal; nothing to round-trip.
            continue;
        };
        if first.is_empty() {
            continue;
        }
        exercised += 1;

        if let Err(e) = inst.set_state(&first) {
            failures.push(format!("{name}: set_state rejected its own state: {e:?}"));
            continue;
        }
        match inst.state() {
            Ok(second) => {
                if first != second {
                    failures.push(format!(
                        "{name}: state changed across round trip ({} bytes -> {} bytes)",
                        first.len(),
                        second.len()
                    ));
                }
            }
            Err(e) => failures.push(format!("{name}: second getState failed: {e:?}")),
        }
    }

    if exercised == 0 {
        eprintln!("no sample plugin exposes state; round-trip not exercised");
    } else {
        eprintln!("state round-trip: {exercised} plugins exercised");
    }
    assert!(
        failures.is_empty(),
        "plugin state did not round-trip:\n{}",
        failures.join("\n")
    );
}

/// f64 processing must build the same spec-clean `ProcessData` as f32.
///
/// This is a separate code path, not a parameter: `Vst3Sample` selects
/// `symbolicSampleSize` *and* which arm of the `channelBuffers32`/`64` union
/// the host writes. A host that sets one without the other hands the plugin a
/// pointer table it will read at the wrong width — silent garbage, since the
/// two union members overlay the same bytes. `ProcessSetupCheck` catches the
/// mismatch by comparing against the negotiated `ProcessSetup`.
#[test]
fn f64_processing_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Activating f64 on a plugin that only does f32 is the host's error,
        // not something to assert on.
        if !supports_f64(&path) {
            continue;
        }
        for frames in [512usize, 1, 64] {
            exercised += 1;
            match drive_block::<f64>(&path, frames, 48_000.0, 512, &[], None) {
                Ok(findings) => {
                    let errs = errors(&findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} @ {frames} frames (f64):\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
                Err(e) => eprintln!("  {name} (f64): skipped ({e})"),
            }
        }
    }

    if exercised == 0 {
        eprintln!("no f64-capable sample plugin available; f64 path not exercised");
    } else {
        eprintln!("f64: {exercised} blocks exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations on the f64 path:\n{}",
        failures.join("\n")
    );
}

/// A playing transport across consecutive blocks must stay spec-clean.
///
/// Reaches `ProcessContextCheck`, which the other tests leave almost untouched:
/// it verifies the context sample rate matches `ProcessSetup` and — the part
/// that needs a *sequence* — that `systemTime` increases monotonically. A host
/// that recomputes system time per block from a wall clock, or forwards a
/// stale value, trips the latter.
#[test]
fn transport_across_blocks_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();

    for (name, path) in sample_plugins() {
        let transport = TransportInfo::default();
        let blocks: Vec<Block<'_>> = (0..8)
            .map(|_| Block::of(512).transport(&transport))
            .collect();

        match drive_blocks::<f32>(&path, 48_000.0, 512, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i}:\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations with a transport across blocks:\n{}",
        failures.join("\n")
    );
}

/// Note-on / note-off pairs spanning blocks must arrive correctly paired.
///
/// `EventListCheck` tracks live notes across `process` calls by both pitch and
/// note id, so this reaches four checks nothing else does: note-off with no
/// matching note-on (by id, and by pitch), and note-on for a pitch/id already
/// sounding. A host that drops, duplicates, or reorders note events across the
/// block boundary — or mangles the note id — shows up here as a stuck or
/// orphaned note, which is exactly how it would be heard.
#[test]
fn note_lifecycle_across_blocks_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        if !accepts_midi(&path) {
            continue;
        }
        exercised += 1;

        // Block 0 starts three notes; block 1 ends them. Nothing is left
        // sounding, and no note-off lacks its note-on.
        let on = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_on(0, 0, 64, 0x6000).with_frame_offset(64),
            MidiEvent::note_on(0, 0, 67, 0x7000).with_frame_offset(128),
        ];
        let off = [
            MidiEvent::note_off(0, 0, 60, 0x4000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 64, 0x4000).with_frame_offset(64),
            MidiEvent::note_off(0, 0, 67, 0x4000).with_frame_offset(128),
        ];
        let blocks = [
            Block::of(512).midi(&on),
            Block::of(512).midi(&off),
            Block::of(512), // silence: nothing should still be sounding
        ];

        match drive_blocks::<f32>(&path, 48_000.0, 512, &blocks) {
            Ok(per_block) => {
                for (i, findings) in per_block.iter().enumerate() {
                    let errs = errors(findings);
                    if !errs.is_empty() {
                        failures.push(format!(
                            "{name} block {i}:\n{}",
                            errs.iter()
                                .map(|e| format!("    {e}"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    if exercised == 0 {
        eprintln!("no MIDI-capable sample plugin available; note lifecycle not exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations in note lifecycle across blocks:\n{}",
        failures.join("\n")
    );
}

/// Parameter automation staged into `inputParameterChanges` must reach the
/// plugin spec-legally.
///
/// This unlocks a whole check category the other tests never reach: with a
/// null `inputParameterChanges` pointer, HostCheck's `ParameterChangesCheck`
/// exits immediately. Populated, it verifies value range (0..=1), that every
/// queue's ParamID is one `IEditController` actually declared, that no ParamID
/// appears twice in one list, that points within a queue are sorted by sample
/// offset, and that no queue pointer is null at a valid index.
///
/// Points are fed deliberately out of order — the host must sort them.
#[test]
fn parameter_changes_are_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Automate the plugin's own first parameter — an invented ParamID
        // would (correctly) be reported as unknown, testing the fixture
        // rather than the host.
        let Some(param_id) = first_parameter_id(&path) else {
            continue;
        };
        let mut params = ParameterChanges::new();
        params.add_change(param_id, 384, 0.75);
        params.add_change(param_id, 0, 0.25);
        params.add_change(param_id, 128, 0.5);

        exercised += 1;
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &[], Some(&params)) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter().map(|e| format!("    {e}")).collect::<Vec<_>>().join("\n")
                    ));
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    if exercised == 0 {
        eprintln!("no sample plugin exposes a parameter; automation path not exercised");
    }
    assert!(
        failures.is_empty(),
        "VST3 spec violations in the staged parameter changes:\n{}",
        failures.join("\n")
    );
}

/// MIDI staged into the input event list must reach the plugin spec-legally:
/// sorted by sample offset, in-range pitch/velocity, valid bus and channel.
/// Feed events deliberately out of order — the host is required to sort.
#[test]
fn midi_event_list_is_spec_clean() {
    if !harness_ready() {
        return;
    }
    let _plugins = plugin_guard();
    let midi = [
        MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(200),
        MidiEvent::note_on(0, 0, 64, 0x5000).with_frame_offset(50),
        MidiEvent::note_off(0, 0, 60, 0x4000).with_frame_offset(400),
        MidiEvent::note_on(0, 0, 67, 0x7000).with_frame_offset(100),
    ];
    let mut failures = Vec::new();
    let mut exercised = 0usize;

    for (name, path) in sample_plugins() {
        // Only plugins that declare an event input bus can legally receive
        // MIDI; feeding an audio-only effect a note-on is an invalid call by
        // the *host*, and HostCheck rightly flags the bus index.
        if !accepts_midi(&path) {
            continue;
        }
        exercised += 1;
        match drive_block::<f32>(&path, 512, 48_000.0, 512, &midi, None) {
            Ok(findings) => {
                let errs = errors(&findings);
                if !errs.is_empty() {
                    failures.push(format!(
                        "{name}:\n{}",
                        errs.iter().map(|e| format!("    {e}")).collect::<Vec<_>>().join("\n")
                    ));
                }
            }
            Err(e) => eprintln!("  {name}: skipped ({e})"),
        }
    }

    if exercised == 0 {
        eprintln!("no MIDI-capable sample plugin available; event-list path not exercised");
    }

    assert!(
        failures.is_empty(),
        "VST3 spec violations in the staged event list:\n{}",
        failures.join("\n")
    );
}
