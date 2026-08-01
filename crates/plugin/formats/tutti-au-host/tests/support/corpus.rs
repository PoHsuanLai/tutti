//! The AU corpus these suites run against, and the rule that absence is loud.
//!
//! ## Why there is no reference plugin here
//!
//! `tutti-clap-host` builds its own reference plugin as a dev-dependency, and
//! `tutti-vst3-host` needs an external SDK checkout. AU needs neither: macOS
//! ships ~30 Apple Audio Units in `/System/Library/Components` as part of the
//! OS, and they cover the shapes a host has to get right — an effect with
//! parameters, one with real reported latency, one that renders silence, and
//! two instruments with no input bus.
//!
//! So the corpus is Apple's own units, addressed by exact four-char type /
//! subtype / manufacturer codes rather than by display-name substring. A name
//! match ("AUDelay") is localized, has been renamed across releases, and would
//! happily bind to a third-party unit that merely contains the substring; the
//! component codes are the stable identifier and are what `auval` itself uses.
//!
//! ## Absence is a hard failure, not a skip
//!
//! [`Corpus::require`] panics when a unit is missing. That is deliberate and it
//! is the same rule `tutti-clap-host`'s `build.rs` documents: these units ship
//! with macOS, so their absence means the environment is broken, not that the
//! machine merely lacks an optional plugin — and a suite that quietly reports
//! `ok` having executed nothing is worse than one that fails.
//!
//! This is not hypothetical. `au_process_no_alloc.rs` returns early with an
//! `eprintln!` when it cannot find AUDelay, which the harness still counts as a
//! pass; the same silent-skip shape let 31 of 32 VST3 conformance tests report
//! success while running nothing at all.

#![cfg(target_os = "macos")]

use tutti_au_host::component::{enumerate_components_of_type, AuComponentInfo, AuType};
use tutti_au_host::instance::AuInstance;
use tutti_au_host::{AuLayoutTag, BusDirection};
use tutti_types::Samples;

/// One Apple Audio Unit, addressed by the codes AudioToolbox registers it under.
#[derive(Debug, Clone, Copy)]
pub struct AuRef {
    /// Display name, for assertion messages only — never used for lookup.
    pub label: &'static str,
    /// Four-char `componentSubType`.
    pub sub_type: &'static [u8; 4],
    /// High-level type, which also selects the enumeration scope.
    pub au_type: AuType,
}

impl AuRef {
    const fn effect(label: &'static str, sub_type: &'static [u8; 4]) -> Self {
        Self {
            label,
            sub_type,
            au_type: AuType::Effect,
        }
    }

    const fn instrument(label: &'static str, sub_type: &'static [u8; 4]) -> Self {
        Self {
            label,
            sub_type,
            au_type: AuType::Instrument,
        }
    }

    const fn mixer(label: &'static str, sub_type: &'static [u8; 4]) -> Self {
        Self {
            label,
            sub_type,
            au_type: AuType::Mixer,
        }
    }

    /// Locate this unit, or `None` if it is not registered on this machine.
    pub fn find(&self) -> Option<AuComponentInfo> {
        let wanted = u32::from_be_bytes(*self.sub_type);
        enumerate_components_of_type(self.au_type)
            .into_iter()
            .find(|c| c.sub_type == wanted && c.manufacturer_code == APPLE)
    }

    /// Locate this unit or panic. See the module docs for why absence is fatal.
    pub fn require(&self) -> AuComponentInfo {
        self.find().unwrap_or_else(|| {
            panic!(
                "{} ({}) is not registered with AudioToolbox. It ships with \
                 macOS, so this means the AU environment is broken rather than \
                 that an optional plugin is missing — see support/corpus.rs.",
                self.label,
                String::from_utf8_lossy(self.sub_type),
            )
        })
    }

    /// Instantiate this unit at `rate`/`block`, already initialized.
    pub fn open(&self, rate: f64, block: u32) -> AuInstance {
        let info = self.require();
        // SAFETY: `component` came from `AudioComponentFindNext` via
        // `enumerate_components_of_type`, so it is a live factory handle for
        // the lifetime of this process.
        let mut au = unsafe { AuInstance::new(info.component, rate, block) }
            .unwrap_or_else(|e| panic!("{}: instantiate failed: {e:?}", self.label));
        au.initialize()
            .unwrap_or_else(|e| panic!("{}: initialize failed: {e:?}", self.label));
        au
    }

    /// Instantiate without initializing, for tests that drive the transition.
    pub fn open_uninitialized(&self, rate: f64, block: u32) -> AuInstance {
        let info = self.require();
        // SAFETY: as in `open`.
        unsafe { AuInstance::new(info.component, rate, block) }
            .unwrap_or_else(|e| panic!("{}: instantiate failed: {e:?}", self.label))
    }
}

/// Apple's `componentManufacturer`, so a third-party unit that happens to reuse
/// a subtype code can never satisfy the corpus.
const APPLE: u32 = u32::from_be_bytes(*b"appl");

/// A delay: many parameters, several units, no reported latency.
pub const DELAY: AuRef = AuRef::effect("AUDelay", b"dely");
/// A 41-parameter EQ — the widest parameter surface in the corpus.
pub const N_BAND_EQ: AuRef = AuRef::effect("AUNBandEQ", b"nbeq");
/// Two parameters only; the minimal effect shape.
pub const LOWPASS: AuRef = AuRef::effect("AULowpass", b"lpas");
/// Reports a real non-zero latency (256 samples of lookahead), so PDC has
/// something to assert against.
pub const DYNAMICS: AuRef = AuRef::effect("AUDynamicsProcessor", b"dcmp");
/// The widest factory-preset surface among Apple's effects (22 presets, named
/// and numbered `0..=21`), and a strongly non-linear processor — so it is the
/// unit that can tell a bypassed render from a processed one at a glance.
pub const DISTORTION: AuRef = AuRef::effect("AUDistortion", b"dist");
/// 13 factory presets, sharing its preset names with [`REVERB2`]. The pair
/// exists so preset assertions are not resting on a single AU's table.
pub const MATRIX_REVERB: AuRef = AuRef::effect("AUMatrixReverb", b"mrev");
/// 13 factory presets. Unlike [`MATRIX_REVERB`] it reports no preset selected
/// (`-1`) on a fresh instance, which is what makes it useful: the two units
/// disagree about the initial state, so nothing may assume one.
pub const REVERB2: AuRef = AuRef::effect("AUReverb2", b"rvb2");
/// A sampler with no input bus, driven by MIDI.
pub const SAMPLER: AuRef = AuRef::instrument("AUSampler", b"samp");
/// The built-in DLS synth: no input bus, driven by MIDI.
pub const DLS_SYNTH: AuRef = AuRef::instrument("DLSMusicDevice", b"dls ");

/// 64 input elements and 4 output elements — the widest bus topology on the
/// system, and the only corpus member with more than one bus on *both* sides.
/// Publishes `{-1,-2}`: any input width, any output width, independently.
pub const MATRIX_MIXER: AuRef = AuRef::mixer("AUMatrixMixer", b"mxmx");
/// 8 input elements, each carrying its own 7-parameter strip. The subject for
/// per-element parameter addressing: the same parameter id on two different
/// input elements holds two independent values.
pub const MULTI_CHANNEL_MIXER: AuRef = AuRef::mixer("AUMultiChannelMixer", b"mcmx");
/// One input, two outputs. The mirror image of DLSMusicDevice — multi-bus on the
/// side the instruments are single-bus on — and it declares `{-1,-1}`, the
/// "any width, but matched" spelling.
pub const MULTI_SPLITTER: AuRef = AuRef::mixer("AUMultiSplitter", b"mspl");
/// A second view-less unit, so the no-editor assertions are not resting on one
/// AU's behaviour. Measured on macOS 15.6, as for [`MATRIX_REVERB`]:
/// `AuEditor::has_editor` is false and `open` fails with
/// `kAudioUnitErr_InvalidProperty` (-10879).
pub const NO_VIEW_SAMPLE_DELAY: AuRef = AuRef::effect("AUSampleDelay", b"sdly");

/// Every effect in the corpus, for tests that assert a property across all of
/// them rather than picking one representative.
pub const EFFECTS: &[AuRef] = &[DELAY, N_BAND_EQ, LOWPASS, DYNAMICS];

/// Effects that ship factory presets, paired with the count each advertises.
///
/// The counts are pinned rather than merely asserted non-empty, because the
/// failure this guards against is a *truncated* enumeration — an off-by-one in
/// the `CFArray` walk, or elements silently dropped by the `filter_map` — and
/// "more than zero presets" would pass all of those. Measured on macOS 15.6;
/// see `au_presets_bypass.rs` for what a mismatch here means.
pub const PRESET_EFFECTS: &[(AuRef, usize)] = &[
    (DISTORTION, 22),
    (MATRIX_REVERB, 13),
    (REVERB2, 13),
    (DYNAMICS, 6),
];

/// Effects whose `kAudioUnitProperty_FactoryPresets` read *fails*, which the
/// host reports as "no presets" rather than as an error. Keeping them named
/// here is what stops that absorption from also hiding a real regression: if
/// one of these ever grew presets the count assertion would catch it.
pub const PRESETLESS_EFFECTS: &[AuRef] = &[DELAY, LOWPASS, N_BAND_EQ];

/// Units measured to advertise a Cocoa view on macOS 15.6, with the frame size
/// each one reported. Sizes are recorded so a host that starts inventing
/// geometry (returning its requested 800x600 rather than the view's own frame)
/// is caught, not merely a host that returns something non-zero.
pub const WITH_COCOA_VIEW: &[(AuRef, u32, u32)] = &[
    (DELAY, 484, 255),
    (LOWPASS, 500, 200),
    (DYNAMICS, 388, 324),
    (N_BAND_EQ, 550, 453),
    (SAMPLER, 793, 596),
    (DLS_SYNTH, 518, 243),
];

/// Units measured to advertise no Cocoa view at all.
pub const WITHOUT_COCOA_VIEW: &[AuRef] = &[MATRIX_REVERB, NO_VIEW_SAMPLE_DELAY];

/// Every instrument in the corpus.
pub const INSTRUMENTS: &[AuRef] = &[SAMPLER, DLS_SYNTH];

/// Every mixer in the corpus. Mixers are where AUv2's multi-bus and per-element
/// parameter features are actually exercised; no effect or instrument on the
/// system uses either.
pub const MIXERS: &[AuRef] = &[MATRIX_MIXER, MULTI_CHANNEL_MIXER, MULTI_SPLITTER];

/// Effects paired with the tail time each reports, in seconds.
///
/// Measured on macOS 15.6 at 48 kHz. Pinned as exact values, not merely
/// "non-zero", because the failure guarded against is reading the *wrong
/// property* — `TailTime` (20) sits next to `Latency` (12) — and these pairs
/// disagree with the latency column everywhere it matters (AUMatrixReverb: 10s
/// tail, 0 latency; AUDynamicsProcessor: 0.2s tail, 256-sample latency), so
/// only the correct property satisfies both. [`NO_TAIL_EFFECTS`] is the
/// zero-tail counterpart.
pub const TAIL_EFFECTS: &[(AuRef, f32)] = &[
    (MATRIX_REVERB, 10.0),
    (REVERB2, 3.0),
    (DYNAMICS, 0.2),
    (N_BAND_EQ, 0.05),
    (DISTORTION, 0.0046),
    (LOWPASS, 0.001),
];

/// Effects measured to report a tail of exactly zero.
///
/// The counterweight to [`TAIL_EFFECTS`]: a genuine zero tail is a different
/// fact from an instrument's refusal to answer at all, and naming one is what
/// stops a host from conflating them — a bounce would otherwise truncate the
/// tail of every unit whose tail it could not read.
pub const NO_TAIL_EFFECTS: &[AuRef] = &[NO_VIEW_SAMPLE_DELAY];

/// Units measured to reject `kAudioUnitProperty_TailTime` outright with
/// `kAudioUnitErr_InvalidProperty` (-10879).
///
/// Every Apple instrument, mixer and generator does. Named here so the host's
/// decision to propagate that as an error rather than flatten it to `Seconds(0)`
/// is pinned by a test — see `au_transport.rs`.
pub const TAILLESS_UNITS: &[AuRef] = &[SAMPLER, DLS_SYNTH, MULTI_CHANNEL_MIXER];

/// The AU tail-time refusal status: `kAudioUnitErr_InvalidProperty`.
pub const TAIL_UNSUPPORTED: i32 = -10879;

/// Planar silence: `channels` buffers of `frames` zeroes.
pub fn silence(channels: usize, frames: usize) -> Vec<Vec<f32>> {
    vec![vec![0.0f32; frames]; channels]
}

/// A unit impulse in channel 0, silence elsewhere.
pub fn impulse(channels: usize, frames: usize) -> Vec<Vec<f32>> {
    let mut b = silence(channels, frames);
    if let Some(first) = b.first_mut() {
        first[0] = 1.0;
    }
    b
}

/// Render one block, borrowing `input` and `output` in the shapes `process`
/// wants. Returns whatever `process` returned so callers can assert on it.
pub fn render(
    au: &mut AuInstance,
    input: &[Vec<f32>],
    output: &mut [Vec<f32>],
    frames: u32,
) -> tutti_au_host::Result<()> {
    let ins: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut outs: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    au.process(&ins, &mut outs, frames)
}

/// Largest absolute sample across every channel.
pub fn peak(buffers: &[Vec<f32>]) -> f32 {
    buffers
        .iter()
        .flat_map(|c| c.iter())
        .fold(0.0f32, |a, &b| a.max(b.abs()))
}

/// True when every sample in every channel is finite (no NaN, no infinity).
pub fn all_finite(buffers: &[Vec<f32>]) -> bool {
    buffers.iter().flat_map(|c| c.iter()).all(|s| s.is_finite())
}

/// Units measured to **accept** a 1-channel stream format on every scope they
/// have, and to `AudioUnitInitialize` at it.
///
/// Measured on macOS 15.6 by setting a mono float32 ASBD and reading it back:
/// these report `mChannelsPerFrame == 1` and initialize with `noErr`. 12 of 15
/// units probed did; the exceptions are in [`REFUSES_MONO`]. Named rather than
/// derived at runtime because a host that silently widened mono to stereo
/// would still pass a test that asked the AU what it supports and then
/// asserted agreement with itself.
///
/// Every member also accepts a 4-channel format, which
/// `a_quad_request_is_honoured_where_the_au_takes_it` relies on.
pub const ACCEPTS_MONO: &[AuRef] = &[DELAY, N_BAND_EQ, LOWPASS, DYNAMICS, SAMPLER];

/// Units measured to **refuse** a 1-channel output format and keep their own
/// width.
///
/// Measured on macOS 15.6: setting a mono output ASBD returns `-10868`
/// (`kAudioUnitErr_FormatNotSupported`) and the read-back still reports 2
/// channels. These are what make `a_refused_layout_reports_the_width_the_au_kept`
/// meaningful — without a unit that genuinely says no, that test could not
/// distinguish an honest report from a lucky one.
///
/// AUMatrixReverb refuses mono but *accepts* quad on its output while keeping 2
/// on its input, so it is also the corpus's asymmetric-layout case.
pub const REFUSES_MONO: &[AuRef] = &[MATRIX_REVERB, DLS_SYNTH];

/// Parameters measured to publish `kAudioUnitProperty_ParameterValueStrings`,
/// as `(unit, parameter id, count, first label, last label)`.
///
/// The count and end labels are pinned, not merely "non-empty", because the
/// failure guarded against is a **truncated or misordered** `CFArray` walk,
/// which an "at least one string" check would still pass. Measured on macOS
/// 15.6: AUNBandEQ publishes the same 11 filter names on each of its 8 band
/// `Type` parameters (ids 2000..=2007; id 2000 is the representative), with
/// `kAudioUnitParameterFlag_ValuesHaveStrings` **clear** — see
/// `au_param_display.rs::value_strings_are_not_gated_on_the_flag_that_under_reports`.
pub const VALUE_STRING_PARAMS: &[(AuRef, u32, usize, &str, &str)] =
    &[(N_BAND_EQ, 2000, 11, "Parametric", "Resonant High Shelf")];

/// Effects measured to group their parameters, paired with the number of distinct
/// clumps **claimed by at least one parameter**.
///
/// Measured on macOS 15.6. Counts are pinned so a host that started reporting
/// `clumpID` unconditionally — collapsing every ungrouped parameter into a
/// phantom clump 0 — is caught, rather than merely one that reports no clumps.
///
/// A non-obvious distinction this count draws, found by a wrong assertion:
/// AUDistortion **names** 7 clumps (1..=7, verified in
/// `distortion_names_its_seven_sections`) but only 6 are claimed by a
/// parameter — nothing carries clump 6 ("Filter"). "Clumps the AU can name"
/// and "clumps the AU actually uses" are different sets, and a UI built from
/// the parameter list renders 6 sections while the AU can label 7. This
/// constant is the *claimed* count, since that is what a section list builds
/// from.
pub const CLUMPED_EFFECTS: &[(AuRef, usize)] = &[(DISTORTION, 6), (MATRIX_REVERB, 4)];

/// Units measured to publish `kAudioUnitParameterFlag_MeterReadOnly` parameters,
/// with the count each advertises.
///
/// These are *readings*, not controls: AUSampler's "Output Amp 0/1" and
/// AUMultibandCompressor's "Comp Amount 1-4" / "Input Amplitude 1-4" /
/// "Output Amplitude 1-4". A host must keep them out of its automation menu, and
/// the count is pinned because the failure mode is under-detection.
///
/// AUMultibandCompressor is not in [`EFFECTS`]; it is referenced only here and by
/// the meter test, since it is the widest meter surface on the system.
pub const METER_PARAM_UNITS: &[(AuRef, usize)] = &[(MULTIBAND_COMPRESSOR, 12), (SAMPLER, 2)];

/// 12 meter pseudo-parameters across 4 bands — the widest `MeterReadOnly` surface
/// among Apple's effects, and 6 parameter clumps.
pub const MULTIBAND_COMPRESSOR: AuRef = AuRef::effect("AUMultibandCompressor", b"mcmp");

/// AUSpatialMixer — in the corpus for two independent reasons, both measured.
///
/// **1. The only corpus unit macOS ships real `.aupreset` *files* for** — 55
/// Apple-authored files under `/System/Library/Audio/Tunings/**/AU/`
/// (`aumx`/`3dem`/`appl`). That makes it the crate's only **interoperability**
/// subject: every other preset assertion round-trips a file this host wrote,
/// proving only self-consistency; loading Apple's own file proves the format
/// itself is right. See [`APPLE_PRESET_DIRS`].
///
/// **2. The only Apple unit advertising `kAudioUnitParameterFlag_CanRamp`, and
/// the proof that flag is a claim, not a guarantee.** Of 155/486 `CanRamp`
/// parameters across 45 units surveyed, 145 are third-party (TDR Nova 37/75,
/// TAL Reverb 4 20/20, TAL-NoiseMaker 88/88); AUSpatialMixer is the entire
/// Apple contribution at 10/12 — and it does **not** honour a ramp: scheduling
/// one across `global reverb gain` (id 9, range -40..40) versus pinning at the
/// start value produces envelopes differing by exactly `0.000000000` over 5
/// runs, while the readback lands on the ramp's end value — the endpoint is
/// applied and the interpolation discarded. See
/// `au_render_notify.rs::the_can_ramp_flag_is_a_claim_not_a_guarantee`.
pub const SPATIAL_MIXER: AuRef = AuRef::mixer("AUSpatialMixer", b"3dem");

/// Directories macOS ships Apple-authored `.aupreset` files in.
///
/// Searched in order and treated as a set rather than a single hardcoded path
/// because the layout is an OS implementation detail: `Generic/AU` holds
/// device-independent presets while the per-tuning `AID*/AU` folders hold
/// hardware-specific ones, and which exist varies with the OS build and
/// attached audio hardware. A test wants *any* genuine Apple preset, so it
/// scans.
///
/// Deliberately NOT `/Library/Audio/Presets/`, the AU-documented user/
/// third-party location: measured to **not exist** on this machine (no
/// `.aupreset` anywhere under `/Library/Audio` or `~/Library/Audio`), because
/// Apple's units ship presets as in-bundle factory presets, not loose files.
/// The Tunings tree is where the loose Apple `.aupreset` files actually are.
pub const APPLE_PRESET_DIRS: &[&str] = &[
    "/System/Library/Audio/Tunings/Generic/AU",
    "/System/Library/Audio/Tunings",
];

/// Locate a genuine Apple-authored `.aupreset` file belonging to `unit`.
///
/// Walks [`APPLE_PRESET_DIRS`] recursively and returns the first file whose
/// **identity keys** name `unit` — matched by parsing the preset, never by its
/// filename. Filenames happen to embed the codes today
/// (`aumx-3dem-appl-headphone-general-stereo.aupreset`), but that is an Apple
/// build-script convention, not part of the format; the same name-versus-codes
/// rule the module docs state for AU lookup applies here.
///
/// Returns `None` when the OS ships no preset for that unit. Unlike a missing
/// *AU*, that is not an environment failure — these files are an
/// implementation detail of Apple's spatial-audio tuning system, not a
/// documented part of macOS — so a caller reports the absence rather than
/// asserting against it.
pub fn find_apple_preset_for(unit: &AuRef) -> Option<std::path::PathBuf> {
    let wanted_sub = u32::from_be_bytes(*unit.sub_type);
    for root in APPLE_PRESET_DIRS {
        let found = walk_aupresets(std::path::Path::new(root))
            .into_iter()
            .find(|p| {
                tutti_au_host::read_preset_metadata(p)
                    .is_ok_and(|id| id.sub_type == wanted_sub && id.manufacturer == APPLE)
            });
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Every `.aupreset` under `dir`, recursively. Depth-limited implicitly by the
/// shallow Tunings tree; errors (unreadable directories) are skipped rather than
/// propagated, because a preset search is best-effort by nature.
fn walk_aupresets(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_aupresets(&path));
        } else if path.extension().is_some_and(|e| e == "aupreset") {
            out.push(path);
        }
    }
    out
}

// ------------------------------------------------- render notify / scheduling

/// `AUSpatialMixer`'s `global reverb gain` — the rampable-but-not-ramped subject.
///
/// `(parameter id, range min, range max)`, measured on macOS 15.6.
pub const SPATIAL_MIXER_RAMP_PARAM: (u32, f32, f32) = (9, -40.0, 40.0);

/// A third-party unit measured to **genuinely** honour a scheduled ramp, and the
/// `Dry` parameter that proves it.
///
/// `(subtype, manufacturer, parameter id)`. Unlike everything else in this
/// module this is looked up **optionally** — see [`optional_third_party`] for the
/// reason absence is tolerated here and nowhere else.
///
/// Measured on macOS 15.6: ramping `Dry` 0.0 → 1.0 across a 512-frame block at
/// 48 kHz, against a 0.5 DC input, yields a strictly monotonic 8-segment output
/// envelope `0.014483 → 0.104773`, where pinning the parameter at the ramp's
/// start value gives a flat `0.0`. Bit-identical across 10 repeats.
pub const TAL_REVERB_4: (&[u8; 4], &[u8; 4], u32) = (b"reV4", b"TOGU", 1_564_260_131);

/// Look up a non-Apple unit, tolerating its absence.
///
/// **This is the one exception to the hard-failure rule** in this module's
/// docs, and it is principled rather than convenient: that rule exists because
/// Apple's units *ship with macOS*, so their absence means a broken
/// environment; a third-party plugin is a genuine optional install, and
/// asserting its presence would fail the suite on any machine that simply
/// lacks it — a false alarm, not a caught regression.
///
/// The discipline that keeps this from becoming the silent skip the module
/// docs warn about: every caller must also assert something
/// **unconditional**, so the test still proves a property when the optional
/// unit is missing. See
/// `au_render_notify.rs::a_ramp_is_honoured_where_a_plugin_implements_it`,
/// which pins the Apple negative result either way and prints a loud notice
/// when the third-party unit is absent.
pub fn optional_third_party(
    sub_type: &[u8; 4],
    manufacturer: &[u8; 4],
    au_type: AuType,
) -> Option<AuComponentInfo> {
    let wanted = u32::from_be_bytes(*sub_type);
    let mfr = u32::from_be_bytes(*manufacturer);
    enumerate_components_of_type(au_type)
        .into_iter()
        .find(|c| c.sub_type == wanted && c.manufacturer_code == mfr)
}

/// Open an arbitrary [`AuComponentInfo`] initialized, for units reached through
/// [`optional_third_party`] rather than an [`AuRef`].
pub fn open_info(info: &AuComponentInfo, rate: f64, block: u32) -> AuInstance {
    // SAFETY: `component` came from `AudioComponentFindNext` via
    // `enumerate_components_of_type`, so it is a live factory handle for the
    // lifetime of this process.
    let mut au = unsafe { AuInstance::new(info.component, rate, block) }
        .unwrap_or_else(|e| panic!("{}: instantiate failed: {e:?}", info.name));
    au.initialize()
        .unwrap_or_else(|e| panic!("{}: initialize failed: {e:?}", info.name));
    au
}

/// Per-segment peak envelope: splits each channel-0 block into `segments` equal
/// spans and reports the peak absolute sample in each.
///
/// This is how an intra-block ramp is distinguished from a step at the block
/// boundary: a ramp's envelope rises across the segments, a step's is flat. A
/// single [`peak`] over the whole block cannot tell them apart — both report the
/// same maximum.
pub fn envelope(buffer: &[f32], segments: usize) -> Vec<f32> {
    let seg = buffer.len() / segments;
    (0..segments)
        .map(|i| {
            buffer[i * seg..(i + 1) * seg]
                .iter()
                .fold(0.0f32, |a, &b| a.max(b.abs()))
        })
        .collect()
}

// ------------------------------------------------- offline / push-render corpus
//
// Everything below was measured on macOS 15.6 by a probe example that has since
// been deleted; the numbers are recorded here because they are the only record.
// Each list is *named* rather than derived at runtime for the reason
// `ACCEPTS_MONO` gives: a test that asks the AU what it supports and then asserts
// agreement with itself passes no matter what the host does.

/// Units measured to implement `kAudioUnitProperty_OfflineRender`.
///
/// **Only the instruments.** Every Apple effect and mixer refuses the property
/// with `kAudioUnitErr_InvalidProperty` (-10879) — for the read, write, and
/// `GetPropertyInfo` alike — a surprising enough shape to spell out: the
/// property whose whole purpose is "this is a bounce, take the slow path" is
/// unimplemented by the units that would most obviously use it. The
/// counterweight, [`WITHOUT_OFFLINE_RENDER`], is what pins the host's decision
/// to propagate that refusal as an error rather than flatten it to `false`.
pub const WITH_OFFLINE_RENDER: &[AuRef] = &[SAMPLER, DLS_SYNTH];

/// Units measured to refuse `kAudioUnitProperty_OfflineRender` outright.
pub const WITHOUT_OFFLINE_RENDER: &[AuRef] = &[
    DELAY,
    DYNAMICS,
    DISTORTION,
    MATRIX_REVERB,
    REVERB2,
    N_BAND_EQ,
    MULTI_CHANNEL_MIXER,
];

/// Units measured to advertise `kAudioUnitProperty_InPlaceProcessing`, with the
/// value each reports.
///
/// The value is pinned rather than merely "the read succeeded" because the
/// property is a capability claim and `1` versus `0` is the whole content of it.
/// Every unit that answers reports `1`; **no unit on this system reports `0`**,
/// which is why the host must not flatten a refusal into `false` — doing so would
/// report that AUMatrixReverb *forbids* in-place operation when it has merely
/// never mentioned it. [`WITHOUT_IN_PLACE`] carries the refusing side.
pub const WITH_IN_PLACE: &[(AuRef, bool)] = &[
    (DELAY, true),
    (DYNAMICS, true),
    (DISTORTION, true),
    (LOWPASS, true),
    (NO_VIEW_SAMPLE_DELAY, true),
    (MULTIBAND_COMPRESSOR, true),
];

/// Units measured to refuse `kAudioUnitProperty_InPlaceProcessing` (-10879).
///
/// Note AUNBandEQ and both reverbs are here while every other effect is in
/// [`WITH_IN_PLACE`]: the split does not follow unit type, so a host cannot infer
/// the capability and must ask.
pub const WITHOUT_IN_PLACE: &[AuRef] = &[
    MATRIX_REVERB,
    REVERB2,
    N_BAND_EQ,
    SAMPLER,
    DLS_SYNTH,
    MULTI_CHANNEL_MIXER,
];

/// Units measured to implement `kAudioUnitProperty_RenderQuality`, paired with
/// the default value each reports on a fresh instance.
///
/// The defaults are pinned because they are not uniform — AUDistortion and
/// AUMultiChannelMixer start at 64, AUMatrixReverb and DLSMusicDevice at 127 —
/// so a host that returned a fabricated constant would satisfy a "non-zero"
/// check but not this.
pub const WITH_RENDER_QUALITY: &[(AuRef, u32)] = &[
    (DISTORTION, 64),
    (MATRIX_REVERB, 127),
    (DLS_SYNTH, 127),
    (MULTI_CHANNEL_MIXER, 64),
];

/// Units measured to refuse `kAudioUnitProperty_RenderQuality` (-10879).
pub const WITHOUT_RENDER_QUALITY: &[AuRef] = &[
    DELAY,
    DYNAMICS,
    REVERB2,
    N_BAND_EQ,
    LOWPASS,
    NO_VIEW_SAMPLE_DELAY,
    SAMPLER,
];

/// The one unit measured to **enforce** the documented 0–127 render-quality
/// range: it answers `paramErr` (-50) for anything above 127 and keeps its
/// previous value.
///
/// Named on its own because it is the exception. AUMatrixReverb, DLSMusicDevice
/// and AUMultiChannelMixer all accept an out-of-range write with `noErr` **and
/// read the out-of-range value straight back** (999 in, 999 out; the mixer
/// round-trips `u32::MAX`). That is what makes the host-side range check
/// load-bearing rather than belt-and-braces, and it is why a write-then-read-back
/// verifier would not substitute for it.
pub const ENFORCES_RENDER_QUALITY_RANGE: AuRef = DISTORTION;

/// `paramErr`, the status [`ENFORCES_RENDER_QUALITY_RANGE`] returns for a
/// render-quality value above 127.
pub const PARAM_ERR: i32 = -50;

/// Units measured to implement the `AudioUnitProcess` push-render selector
/// (`noErr`, and audio comes out).
///
/// 6 of the 9 corpus effects. AUMatrixReverb and AUReverb2 do not, nor does any
/// instrument or mixer — see [`WITHOUT_PUSH_RENDER`].
pub const WITH_PUSH_RENDER: &[AuRef] = &[
    DELAY,
    DYNAMICS,
    DISTORTION,
    N_BAND_EQ,
    LOWPASS,
    NO_VIEW_SAMPLE_DELAY,
    MULTIBAND_COMPRESSOR,
];

/// Units measured to answer `unimpErr` to `AudioUnitProcess`.
///
/// The component manager's "selector not implemented", not a render failure.
/// Named so the host's refusal path is exercised against a real refusal rather
/// than a fabricated one — the same discipline [`REFUSES_MONO`] applies to stream
/// formats.
pub const WITHOUT_PUSH_RENDER: &[AuRef] = &[
    MATRIX_REVERB,
    REVERB2,
    SAMPLER,
    DLS_SYNTH,
    MULTI_CHANNEL_MIXER,
];

/// The **only** unit on this system that implements `AudioUnitProcessMultiple`.
///
/// It accepts exactly one input buffer list: a second is refused with
/// `kAudioUnitErr_InvalidElement` (-10877), correct since AUReverb2 has one
/// input element. Every other unit — including AUMultiChannelMixer with **8**
/// real input elements — answers `unimpErr` for 1, 2 and 8 lists alike, so the
/// absence is the selector, not the topology.
///
/// The headline finding this records: **there is no working AU sidechain on
/// this machine.** `AudioUnitProcessMultiple` is the only AUv2 call that can
/// carry a second input bus, and nothing implements it in a form that accepts
/// one.
pub const IMPLEMENTS_PROCESS_MULTIPLE: AuRef = REVERB2;

/// `unimpErr` — the status an AU returns for a dispatch selector it does not
/// implement. Re-exported from the host crate rather than re-spelled so the two
/// cannot drift.
pub const UNIMP_ERR: i32 = tutti_au_host::types::UNIMP_ERR;

/// `kAudioUnitErr_TooManyFramesToProcess`, the AU's own answer to a render wider
/// than the `MaximumFramesPerSlice` it was initialized at.
///
/// Pinned so the host's `InvalidBuffer` guard can be shown to fire *instead of*
/// this, not merely *alongside* it: the host must refuse an oversized render
/// before handing the AU a buffer list whose `mDataByteSize` overstates
/// storage it actually allocated.
pub const TOO_MANY_FRAMES: i32 = -10874;

/// A 440 Hz sine at `amplitude`, `channels` wide, starting at sample `offset`.
///
/// Deliberately not silence: an in-place render path can be wrong in ways
/// silence cannot reveal — reading its own output as input, or emitting the
/// previous block — and every such failure still produces zeroes when fed
/// zeroes.
pub fn sine(
    channels: usize,
    frames: usize,
    offset: usize,
    amplitude: f32,
    rate: f32,
) -> Vec<Vec<f32>> {
    (0..channels)
        .map(|_| {
            (0..frames)
                .map(|i| {
                    let n = (offset + i) as f32;
                    (2.0 * std::f32::consts::PI * 440.0 * n / rate).sin() * amplitude
                })
                .collect()
        })
        .collect()
}

// ------------------------------------------------- channel layout / element name

/// The widest `SupportedChannelLayoutTags` surface among Apple's effects — 12
/// entries on each scope, including a duplicate. See [`DUPLICATE_TAG_UNIT`].
pub const NEW_PITCH: AuRef = AuRef::effect("AUNewPitch", b"nutp");
/// 8 layout tags on both scopes, all literal widths with no wildcards, and the
/// only Apple effect that publishes a full `Mono`→`Octagonal` ladder.
pub const ROUND_TRIP_AAC: AuRef = AuRef::effect("AURoundTripAAC", b"raac");
/// A second instrument publishing `[Mono, Stereo]`, so the layout-tag assertions
/// on [`SAMPLER`] are not resting on one AU's table.
pub const MIDI_SYNTH: AuRef = AuRef::instrument("AUMIDISynth", b"msyn");

/// Units measured to publish `kAudioUnitProperty_SupportedChannelLayoutTags`, as
/// `(unit, direction-is-output, expected tags)`.
///
/// Measured on macOS 15.6 at 48 kHz / 512 frames, in the `Loaded` state. The
/// exact tag *sequences* are pinned rather than merely "non-empty", for the
/// reason [`VALUE_STRING_PARAMS`] pins its counts: a truncated or misordered
/// array walk would still pass an "at least one tag" check.
///
/// Only 11 of the ~38 units probed answer this property at all. The refusers —
/// AUDelay, AUNBandEQ, AUMatrixMixer and the rest, all `-10879` — are in
/// [`NO_LAYOUT_TAG_UNITS`]. See [`DUPLICATE_TAG_UNIT`] for AUNewPitch's row,
/// split out on its own.
pub const LAYOUT_TAG_UNITS: &[(AuRef, bool, &[AuLayoutTag])] = &[
    // AUMatrixReverb: output only; its input scope refuses the property. This is
    // the unit the set/refuse assertions use, because it is the only Apple effect
    // publishing surround tags.
    (
        MATRIX_REVERB,
        true,
        &[
            AuLayoutTag::Stereo,
            AuLayoutTag::Quadraphonic,
            AuLayoutTag::AudioUnit5_0,
        ],
    ),
    // Both instruments, output only: the narrowest real table on the system.
    (SAMPLER, true, &[AuLayoutTag::Mono, AuLayoutTag::Stereo]),
    (MIDI_SYNTH, true, &[AuLayoutTag::Mono, AuLayoutTag::Stereo]),
    // AURoundTripAAC: the same 8 tags on both scopes, ordered by width except
    // that MPEG_3_0_A precedes Quadraphonic.
    (
        ROUND_TRIP_AAC,
        false,
        &[
            AuLayoutTag::Mono,
            AuLayoutTag::Stereo,
            AuLayoutTag::Unknown(0x0071_0003),
            AuLayoutTag::Quadraphonic,
            AuLayoutTag::AudioUnit5_0,
            AuLayoutTag::AudioUnit6_0,
            AuLayoutTag::AudioUnit7_0,
            AuLayoutTag::Octagonal,
        ],
    ),
    (
        ROUND_TRIP_AAC,
        true,
        &[
            AuLayoutTag::Mono,
            AuLayoutTag::Stereo,
            AuLayoutTag::Unknown(0x0071_0003),
            AuLayoutTag::Quadraphonic,
            AuLayoutTag::AudioUnit5_0,
            AuLayoutTag::AudioUnit6_0,
            AuLayoutTag::AudioUnit7_0,
            AuLayoutTag::Octagonal,
        ],
    ),
    // AUMultiChannelMixer: one entry, and it is the "I want descriptions, not a
    // tag" sentinel — which is why `AuLayoutTag::channel_count` must report `None`
    // for it rather than a plausible-looking zero.
    (
        MULTI_CHANNEL_MIXER,
        false,
        &[AuLayoutTag::UseChannelDescriptions],
    ),
    (
        MULTI_CHANNEL_MIXER,
        true,
        &[AuLayoutTag::UseChannelDescriptions],
    ),
];

/// Units measured to publish the duplicate-entry table, with the duplicate.
///
/// Split out from [`LAYOUT_TAG_UNITS`] because AUNewPitch's 12-entry list is the
/// one row whose point is the *duplicate*, and burying it among the others would
/// let a reader take it for a typo.
pub const DUPLICATE_TAG_UNIT: (AuRef, &[AuLayoutTag]) = (
    NEW_PITCH,
    &[
        AuLayoutTag::Mono,
        AuLayoutTag::Stereo,
        AuLayoutTag::Quadraphonic,
        // Again — `AudioUnit_4` under its other spelling, the same value.
        AuLayoutTag::Quadraphonic,
        AuLayoutTag::Pentagonal,
        AuLayoutTag::Hexagonal,
        AuLayoutTag::Octagonal,
        AuLayoutTag::AudioUnit5_0,
        AuLayoutTag::AudioUnit6_0,
        AuLayoutTag::AudioUnit7_0,
        AuLayoutTag::Unknown(0x0094_0007),
        AuLayoutTag::UseChannelDescriptions,
    ],
);

/// Units measured to refuse `SupportedChannelLayoutTags` outright with
/// `kAudioUnitErr_InvalidProperty` (-10879) on **both** scopes.
///
/// The counterweight to [`LAYOUT_TAG_UNITS`]: the host absorbs the refusal into an
/// empty vec, and naming the refusers is what stops that absorption from also
/// hiding a real regression. If one of these ever grew tags, the emptiness
/// assertion catches it.
pub const NO_LAYOUT_TAG_UNITS: &[AuRef] = &[DELAY, N_BAND_EQ, LOWPASS, MATRIX_MIXER];

/// `(unit, is_output, configured width, tag)` triples measured to be **accepted**
/// by `AudioUnitSetProperty(AudioChannelLayout)`.
///
/// The width is load-bearing and is why it is in the tuple: the AU checks the
/// tag's channel count against the width already configured on that bus. Every
/// row here was verified to return `noErr` *and* to read back the tag that was
/// written. See [`REFUSED_LAYOUT_SETS`] for the mismatches.
pub const ACCEPTED_LAYOUT_SETS: &[(AuRef, bool, u16, AuLayoutTag)] = &[
    (MATRIX_REVERB, true, 2, AuLayoutTag::Stereo),
    (MATRIX_REVERB, true, 4, AuLayoutTag::Quadraphonic),
    (MATRIX_REVERB, true, 5, AuLayoutTag::AudioUnit5_0),
    (SAMPLER, true, 2, AuLayoutTag::Stereo),
    (SAMPLER, true, 1, AuLayoutTag::Mono),
];

/// `(unit, is_output, configured width, tag)` triples measured to be **refused**
/// with `kAudioUnitErr_InvalidPropertyValue` (-10851).
///
/// Two distinct reasons are represented, and both matter:
///
/// * a tag the AU **does** publish, at a width that does not match it —
///   `AudioUnit_5_0` (5 channels) on a 4-channel bus, `Quadraphonic` (4) on a
///   5-channel bus. These are what make [`ACCEPTED_LAYOUT_SETS`]'s width column
///   meaningful: without them the acceptance could be luck.
/// * a tag the AU does **not** publish at all — `AudioUnit_5_1` on
///   AUMatrixReverb, `Quadraphonic` on AUSampler.
pub const REFUSED_LAYOUT_SETS: &[(AuRef, bool, u16, AuLayoutTag)] = &[
    // Published, wrong width.
    (MATRIX_REVERB, true, 4, AuLayoutTag::AudioUnit5_0),
    (MATRIX_REVERB, true, 5, AuLayoutTag::Quadraphonic),
    (MATRIX_REVERB, true, 2, AuLayoutTag::Quadraphonic),
    // Not published at any width.
    (MATRIX_REVERB, true, 6, AuLayoutTag::AudioUnit5_1),
    (SAMPLER, true, 2, AuLayoutTag::Quadraphonic),
];

/// Units measured to report a **current** `AudioChannelLayout`, with the tag each
/// reports on a freshly-instantiated stereo instance.
///
/// Every one reports `Stereo`, which is not a tautology worth skipping: the point
/// is that the tag is read out of the AU rather than inferred from the width this
/// host configured. A host that returned `ChannelLayout::from(count)` dressed up as
/// a tag would also pass this — which is why [`ACCEPTED_LAYOUT_SETS`] then
/// *changes* the tag and re-reads it.
pub const CURRENT_LAYOUT_UNITS: &[(AuRef, bool, AuLayoutTag)] = &[
    (MATRIX_REVERB, true, AuLayoutTag::Stereo),
    (MULTI_CHANNEL_MIXER, false, AuLayoutTag::Stereo),
    (MULTI_CHANNEL_MIXER, true, AuLayoutTag::Stereo),
    (SPATIAL_MIXER, true, AuLayoutTag::Stereo),
];

/// Units measured to answer `kAudioUnitProperty_AudioChannelLayout` with
/// `kAudioUnitErr_PropertyNotInUse` (-10851) — the property exists, no value set.
///
/// AUSampler's output is the case Apple's header describes ("Requesting the value
/// of this property when it is implemented but not set results in a
/// kAudioUnitErr_PropertyNotInUse error") and it is the reason
/// `AuInstance::layout_tag` returns `Result` rather than an `Option` or a default:
/// this unit *does* publish `[Mono, Stereo]` as supported, so "no tags" cannot be
/// used to predict it, and a fabricated `Stereo` would be a claim about speaker
/// order the host cannot back up.
pub const LAYOUT_NOT_IN_USE_UNITS: &[(AuRef, bool)] = &[(SAMPLER, true)];

/// Units measured to have **no** channel-layout property on either scope
/// (`kAudioUnitErr_InvalidProperty`, -10879).
pub const NO_LAYOUT_UNITS: &[AuRef] = &[DELAY, N_BAND_EQ, LOWPASS, MATRIX_MIXER];

/// `(unit, is_output, element, name)` quadruples measured to publish an element
/// name.
///
/// **Apple's mixers are not here, and that is the finding.** AUMultiChannelMixer
/// and AUMatrixMixer answer `kAudioUnitErr_PropertyNotInUse` (-10850) for every
/// one of their real input elements — 0..=7 and 0..=63 respectively — so on
/// macOS 15.6 Apple's mixers publish no element names at all. DLSMusicDevice is
/// the one Apple unit that does, which is this constant's own weight; the
/// third-party effects (named in [`THIRD_PARTY_NAMED_ELEMENTS`]) add a second
/// named element and a sidechain to distinguish it from.
pub const NAMED_ELEMENTS_APPLE_ONLY: &[(AuRef, bool, u32, &str)] = &[
    // Its second output is literally called "unused" — the kind of thing a host
    // should show the user rather than rendering as "Bus 2".
    (DLS_SYNTH, true, 0, "stereo mix"),
    (DLS_SYNTH, true, 1, "unused"),
];

/// `(unit, is_output, element)` pairs measured to answer -10850
/// (`PropertyNotInUse`) — a **real** bus the AU gave no name.
///
/// Distinct from [`OUT_OF_RANGE_ELEMENTS`], which is -10877. That split is the
/// whole reason `element_name` returns the AU's status instead of an empty string:
/// a nameless bus and a nonexistent bus are different facts, and a host sizing
/// buffers cannot confuse them.
pub const UNNAMED_ELEMENTS: &[(AuRef, bool, u32)] = &[
    // Every real input of both Apple mixers.
    (MULTI_CHANNEL_MIXER, false, 0),
    (MULTI_CHANNEL_MIXER, false, 7),
    (MATRIX_MIXER, false, 0),
    (MATRIX_MIXER, false, 63),
    (MATRIX_MIXER, true, 0),
];

/// `(unit, is_output, element)` pairs measured to answer -10877
/// (`InvalidElement`) — no such bus.
///
/// Each index is exactly **one past** the unit's real bus count, which is what
/// makes these a boundary test rather than a "999 fails" formality:
/// AUMultiChannelMixer has 8 inputs so input 8 is the first invalid one, and
/// AUMatrixMixer has 64 so input 64 is. A host that clamped an out-of-range index
/// to the last valid bus would return "the name of input 7" for input 8 and pass
/// any test that only tried 999.
pub const OUT_OF_RANGE_ELEMENTS: &[(AuRef, bool, u32)] = &[
    (MULTI_CHANNEL_MIXER, false, 8),
    (MATRIX_MIXER, false, 64),
    (DLS_SYNTH, true, 2),
];

/// The AU status for "the property exists here but holds no value":
/// `kAudioUnitErr_PropertyNotInUse`.
pub const PROPERTY_NOT_IN_USE: i32 = -10850;
/// The AU status for "no such element": `kAudioUnitErr_InvalidElement`.
pub const INVALID_ELEMENT: i32 = -10877;
/// The AU status for "no such property on this unit":
/// `kAudioUnitErr_InvalidProperty`.
pub const INVALID_PROPERTY: i32 = -10879;
/// The AU status for a value the property will not take:
/// `kAudioUnitErr_InvalidPropertyValue`.
pub const INVALID_PROPERTY_VALUE: i32 = -10851;

/// Every AU installed on this machine, for the exhaustive MIDI-output sweep.
///
/// `au_midi_out.rs` asserts a *negative* — that no unit publishes
/// `MIDIOutputCallbackInfo` — and a negative asserted over a hand-picked corpus
/// proves nothing. This walks the whole registry so the claim is about the machine
/// rather than about five chosen units.
pub fn every_component() -> Vec<AuComponentInfo> {
    tutti_au_host::component::enumerate_components()
}

/// The `is_output` booleans the layout tables above carry, as a [`BusDirection`].
///
/// A `bool` in the tables rather than the enum, because a `const` table of
/// `(AuRef, BusDirection, …)` cannot be written in one literal without naming the
/// enum path at every row; this keeps the tables readable and puts the conversion
/// in one place.
pub fn direction(is_output: bool) -> BusDirection {
    if is_output {
        BusDirection::Output
    } else {
        BusDirection::Input
    }
}

/// Instantiate `unit` (uninitialized) with its output bus configured to `width`
/// channels, and report the width the AU actually accepted.
///
/// The width is what gates every `AudioChannelLayout` *write* — an AU refuses a
/// tag whose channel count disagrees with the configured bus width — so a layout
/// test that could not set the width could only ever exercise stereo. Uses
/// `new_with_config`, which is the entry point that bypasses `new`'s stereo
/// default.
///
/// Returns `(instance, accepted_width)` rather than asserting the width took:
/// several units refuse a narrowing and keep their own, and a caller asserting on
/// a layout needs to know which it got. `has_input` is taken from the AU's own
/// probe by way of `AuInstance`, so an instrument is not handed a phantom input.
pub fn open_at_output_width(unit: &AuRef, rate: f64, block: u32, width: u16) -> (AuInstance, u16) {
    use tutti_au_host::stream::{AuBusLayout, StreamConfig};
    use tutti_types::ChannelLayout;

    let info = unit.require();
    // Probe `has_input` from a throwaway default instance: `StreamConfig` needs
    // the flag up front, and getting it wrong sends an input stream format to a
    // unit with no input element.
    let has_input = {
        // SAFETY: `component` came from `AudioComponentFindNext`.
        let probe = unsafe { AuInstance::new(info.component, rate, block) }
            .unwrap_or_else(|e| panic!("{}: probe instantiate failed: {e:?}", unit.label));
        probe.num_inputs() > 0
    };
    let config = StreamConfig::new(
        rate,
        block,
        AuBusLayout {
            inputs: ChannelLayout::STEREO,
            outputs: ChannelLayout::from_count(width),
            has_input,
        },
    );
    // SAFETY: as above.
    let au = unsafe { AuInstance::new_with_config(info.component, config) }
        .unwrap_or_else(|e| panic!("{}: instantiate at {width}ch failed: {e:?}", unit.label));
    let accepted = au.num_outputs() as u16;
    (au, accepted)
}

// ------------------------------------------------- third-party corpus
//
// Everything below addresses a **non-Apple** AU, and so is reached through
// [`optional_third_party`] rather than [`AuRef::require`]: these are a genuine
// optional install, and the hard-failure rule above exists precisely because
// Apple's units are not. See `au_third_party.rs`'s module docs for the
// absence-handling contract every caller here owes.
//
// All figures measured on macOS 15.6 at 48 kHz / 512 frames by a probe example
// that has since been deleted; these constants are the only record. Every code
// was verified against `auval -a` output before being written down — a previous
// attempt at this corpus guessed the subtypes and matched nothing at all, which
// only a skip guard caught.

/// One third-party AU, addressed by the codes `auval -a` prints for it.
///
/// Deliberately a separate type from [`AuRef`] rather than a manufacturer field
/// on it: `AuRef::require` panics on absence and every Apple-facing test depends
/// on that, so a third-party unit must not be reachable through the same API. The
/// only way to open one of these is [`ThirdPartyRef::find`], which returns an
/// `Option` the caller is forced to handle.
#[derive(Debug, Clone, Copy)]
pub struct ThirdPartyRef {
    /// Display name, for assertion messages and skip notices only.
    pub label: &'static str,
    /// Four-char `componentSubType`, as printed by `auval -a`.
    pub sub_type: &'static [u8; 4],
    /// Four-char `componentManufacturer`, as printed by `auval -a`.
    pub manufacturer: &'static [u8; 4],
    /// High-level type, which also selects the enumeration scope.
    pub au_type: AuType,
}

impl ThirdPartyRef {
    /// Locate this unit, or `None` when it is not installed on this machine.
    pub fn find(&self) -> Option<AuComponentInfo> {
        optional_third_party(self.sub_type, self.manufacturer, self.au_type)
    }
}

/// TDR Nova — `aufx`/`Td5a`/`Tdrl`, Tokyo Dawn Labs.
///
/// The corpus's **JUCE** subject, and the only reason the crate's
/// `relax-void-encoding` feature can be shown to be load-bearing: JUCE declares
/// the Cocoa view factory's AudioUnit argument as
/// `^{ComponentInstanceRecord=[1q]}` where Apple declares
/// `^{OpaqueAudioComponentInstance=}`, so objc2's debug encoding check rejects one
/// of the two unless pointer identity is relaxed. No Apple unit exercises that
/// path.
///
/// Measured: 75 parameters (ids 48..=1757, non-contiguous), **184 samples of
/// reported latency** — the largest in the whole corpus, Apple included — a 4246
/// byte state blob, 73 factory presets, and a 830x598 Cocoa view.
pub const TDR_NOVA: ThirdPartyRef = ThirdPartyRef {
    label: "TDR Nova",
    sub_type: b"Td5a",
    manufacturer: b"Tdrl",
    au_type: AuType::Effect,
};

/// TAL-NoiseMaker — `aumu`/`ncut`/`TOGU`, TAL-Togu Audio Line.
///
/// Two facts no Apple unit shows:
///
/// * **an instrument that reports 2 input channels.** Every Apple instrument
///   reports 0 and the host had to be taught not to install an input callback on
///   one; this unit is the opposite shape, and a host that infers "instrument
///   therefore no input" from the type code rather than asking gets it wrong here.
/// * **a mortal `ElementName` string.** Its `out[0]` name "Output Master" has a
///   real retain count of 2, so an unreleased read walks it 2→11 across 10 reads.
///   Apple's units all return immortal strings (`u64::MAX` or
///   `0x0FFF_FFFF_FFFF_FFFF`), which hides the leak completely.
///
/// Measured: 88 parameters (ids 0..=87, all `CanRamp`), a 3189 byte state blob,
/// **zero** factory presets, and an 800x437 Cocoa view.
pub const TAL_NOISEMAKER: ThirdPartyRef = ThirdPartyRef {
    label: "TAL-NoiseMaker",
    sub_type: b"ncut",
    manufacturer: b"TOGU",
    au_type: AuType::Instrument,
};

/// TAL Reverb 4 — `aufx`/`reV4`/`TOGU`, TAL-Togu Audio Line.
///
/// The unit that reports an **infinite tail time**: `kAudioUnitProperty_TailTime`
/// answers `f64::INFINITY`, which no Apple unit does (their maxima are ~21 s). See
/// [`INFINITE_TAIL_UNIT`] for why that single value is worth a named constant.
///
/// Also the corpus's only genuine parameter-ramp implementer — that leg lives in
/// `au_render_notify.rs` via [`TAL_REVERB_4`], which predates this block.
///
/// Measured: 20 parameters (all `CanRamp`), a 1070 byte state blob, 1 factory
/// preset ("default"), and zero reported latency.
pub const TAL_REVERB_4_REF: ThirdPartyRef = ThirdPartyRef {
    label: "TAL Reverb 4",
    sub_type: b"reV4",
    manufacturer: b"TOGU",
    au_type: AuType::Effect,
};

/// Every third-party unit in the corpus, for tests that assert a property across
/// all of them rather than picking one representative.
pub const THIRD_PARTY: &[ThirdPartyRef] = &[TDR_NOVA, TAL_NOISEMAKER, TAL_REVERB_4_REF];

/// `(unit, parameter count)` pairs, measured.
///
/// Pinned rather than merely "non-empty" for the reason [`PRESET_EFFECTS`] pins
/// its counts: the failure guarded against is a **truncated** parameter-list walk,
/// and "more than zero parameters" passes every off-by-one. 75 and 88 are both
/// wider than any Apple unit in the corpus (AUNBandEQ's 41 is the widest), so
/// these are also the only rows that exercise a parameter id list long enough to
/// span a `CFArray` realloc.
pub const THIRD_PARTY_PARAM_COUNTS: &[(ThirdPartyRef, usize)] =
    &[(TDR_NOVA, 75), (TAL_NOISEMAKER, 88), (TAL_REVERB_4_REF, 20)];

/// The one unit measured to report a **non-finite** tail time.
///
/// `kAudioUnitProperty_TailTime` answers `f64::INFINITY` — an honest claim from
/// a reverb with infinite decay, and a value no Apple unit produces. Named
/// because of what happens downstream: `Seconds::to_samples` maps every
/// non-finite input to `Samples::ZERO`, so an infinite tail silently becomes
/// *no tail at all* — a bounce would truncate the reverb rather than render it
/// forever. Both halves (host propagates `inf` faithfully; unit conversion
/// collapses it) are asserted in
/// `au_third_party.rs::an_infinite_tail_survives_the_host_and_collapses_in_conversion`.
pub const INFINITE_TAIL_UNIT: ThirdPartyRef = TAL_REVERB_4_REF;

/// `(unit, reported latency in samples at 48 kHz)` pairs, measured.
///
/// TDR Nova's 184 samples is the largest reported latency in the corpus —
/// AUDynamicsProcessor's 256 is a *lookahead* the unit reports at a different
/// property — and it is **rate-independent**: measured 184 at both 48 kHz and
/// 44.1 kHz, because the plugin reports a fixed sample count rather than a fixed
/// duration. That is the opposite of what the property's `Float64`-seconds
/// encoding suggests, so it is worth pinning: a host that recomputed PDC from a
/// cached seconds value on a rate change would drift here.
/// `(unit, version string)` pairs, cross-checked against each bundle's
/// `CFBundleShortVersionString`.
///
/// The point of pinning third-party units rather than Apple's: Apple ships every
/// AU at the same OS version (`1.6.0` across the whole corpus), so an encoding
/// bug that swapped or dropped a field would still agree with itself on all of
/// them. These three have distinct majors, minors and dots — `2.2.2`, `5.0.6`,
/// `4.0.4` — so a mis-shifted decode disagrees visibly.
///
/// They are also what establishes the byte layout. `AudioComponentGetVersion`
/// returns `0x00020202` / `0x00050006` / `0x00040004`, and reading the top half
/// as a 16-bit major (as the header's `0xMMMMmmDD` suggests) agrees on these
/// only because their majors are single-digit. `Info.plist` is the independent
/// source that settles it.
pub const THIRD_PARTY_VERSION: &[(ThirdPartyRef, &str)] = &[
    (TDR_NOVA, "2.2.2"),
    (TAL_NOISEMAKER, "5.0.6"),
    (TAL_REVERB_4_REF, "4.0.4"),
];

pub const THIRD_PARTY_LATENCY: &[(ThirdPartyRef, Samples)] = &[
    (TDR_NOVA, Samples(184)),
    (TAL_NOISEMAKER, Samples::ZERO),
    (TAL_REVERB_4_REF, Samples::ZERO),
];

/// `(unit, expected Cocoa view width, height)` triples, measured.
///
/// Bit-stable across 3 runs and 8 open/close cycles. Sizes are pinned for the
/// reason [`WITH_COCOA_VIEW`] pins Apple's: a host that started inventing
/// geometry — returning its own requested size rather than the view's frame —
/// is caught, not merely one that returns something non-zero.
///
/// TDR Nova's row is the JUCE one, and the only evidence in the suite that
/// `relax-void-encoding` works against a plugin that declares the argument
/// differently from Apple.
pub const THIRD_PARTY_COCOA_VIEW: &[(ThirdPartyRef, u32, u32)] =
    &[(TDR_NOVA, 830, 598), (TAL_NOISEMAKER, 800, 437)];

/// `(unit, is_output, element, name)` quadruples measured to publish an element
/// name.
///
/// The third-party half of [`NAMED_ELEMENTS_APPLE_ONLY`], and the interesting
/// half: Apple's mixers name nothing, so without these the only named element on
/// the machine is DLSMusicDevice's. Note TDR Nova and TAL Reverb 4 both name a
/// **"Sidechain"** input — a second input bus that Apple's corpus does not
/// provide at all, and which a host must be able to tell apart from a second
/// audio input.
pub const THIRD_PARTY_NAMED_ELEMENTS: &[(ThirdPartyRef, bool, u32, &str)] = &[
    (TDR_NOVA, false, 0, "Input"),
    (TDR_NOVA, false, 1, "Sidechain"),
    (TDR_NOVA, true, 0, "Output"),
    (TAL_REVERB_4_REF, false, 0, "Input"),
    (TAL_REVERB_4_REF, false, 1, "Sidechain"),
    (TAL_REVERB_4_REF, true, 0, "Output"),
    // The mortal one — see `TAL_NOISEMAKER`'s docs.
    (TAL_NOISEMAKER, true, 0, "Output Master"),
];

/// The element whose `CFStringRef` is **mortal**, and the retain count a fresh
/// read reports.
///
/// `(unit, is_output, element, base retain count)`. Measured: reading
/// TAL-NoiseMaker's `out[0]` name ten times *without* releasing walks the count
/// `2,3,4,…,11`; reading it through the host holds it flat. This is the only
/// element on the machine that can distinguish a releasing host from a leaking
/// one — see
/// `au_third_party.rs::the_element_name_copy_rule_is_observed_by_retain_count`.
pub const MORTAL_ELEMENT_NAME: (ThirdPartyRef, bool, u32, usize) = (TAL_NOISEMAKER, true, 0, 2);

/// `(unit, factory preset count)` pairs, measured.
///
/// TDR Nova's 73 is more than triple AUDistortion's 22, the widest Apple table,
/// and its last preset is number 72 ("USER060") — so this is the only row that
/// exercises a preset selector well outside the range Apple's units use.
/// TAL-NoiseMaker's **zero** is the counterweight: an AU whose factory-preset
/// read succeeds and reports an empty table, which is a different fact from the
/// read failing (see [`PRESETLESS_EFFECTS`], where it fails).
pub const THIRD_PARTY_PRESET_COUNTS: &[(ThirdPartyRef, usize)] =
    &[(TDR_NOVA, 73), (TAL_REVERB_4_REF, 1), (TAL_NOISEMAKER, 0)];

/// Units measured to answer `unimpErr` (-4) to **`AudioUnitProcess`**.
///
/// All three, which is the finding: [`WITH_PUSH_RENDER`] shows 7 Apple effects
/// implementing the push selector, so a host might reasonably conclude it is the
/// normal path for an effect. Every third-party unit installed refuses it. A host
/// that used `process_push` as its only render path would be silent on all real
/// plugins.
pub const THIRD_PARTY_WITHOUT_PUSH_RENDER: &[ThirdPartyRef] =
    &[TDR_NOVA, TAL_NOISEMAKER, TAL_REVERB_4_REF];

/// Units measured to refuse `kAudioUnitProperty_InPlaceProcessing` **and**
/// `kAudioUnitProperty_RenderQuality`, both with -10879.
///
/// All three. Contrast [`WITH_IN_PLACE`], where 6 Apple units answer the first,
/// and [`WITH_RENDER_QUALITY`], where 4 answer the second: neither property is
/// something a host can count on from a real plugin.
pub const THIRD_PARTY_WITHOUT_OPTIONAL_PROPS: &[ThirdPartyRef] =
    &[TDR_NOVA, TAL_NOISEMAKER, TAL_REVERB_4_REF];

/// TAL-NoiseMaker's idle output floor, measured over 4 blocks with no notes.
///
/// **Not zero** — 1.12e-7, bit-reproducible. The unit emits a tiny amount of
/// noise even at rest, so an "instrument is silent until played" assertion has to
/// use a threshold. Recorded because the obvious `== 0.0` form would fail here for
/// a reason that has nothing to do with the host.
pub const NOISEMAKER_IDLE_FLOOR: f32 = 1.2e-7;

/// The peak TAL-NoiseMaker reaches within 16 blocks of a note-on at velocity
/// `0xC000`, measured 0.28133097 and bit-reproducible across runs.
pub const NOISEMAKER_NOTE_PEAK: f32 = 0.28133097;
