//! The AU corpus these suites run against, and the rule that absence is loud.
//!
//! ## Why there is no reference plugin here
//!
//! `tutti-clap-host` builds its own reference plugin as a dev-dependency, and
//! `tutti-vst3-host` needs an external SDK checkout. AU needs neither: macOS
//! ships ~30 Apple Audio Units in `/System/Library/Components`, they are part
//! of the OS rather than an optional install, and they cover the shapes a host
//! has to get right — an effect with parameters, one with real reported
//! latency, one that renders silence, and two instruments with no input bus.
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
/// Measured on macOS 15.6 at 48 kHz. Pinned as exact values rather than merely
/// "non-zero" because the failure this guards against is reading the *wrong
/// property*: `kAudioUnitProperty_TailTime` (20) sits next to `Latency` (12) and
/// several other `Float64` global-scope properties, and a host that read latency
/// by mistake would still get a plausible non-zero float out of
/// AUDynamicsProcessor. The pairs below disagree with the latency column
/// everywhere it matters — AUMatrixReverb reports 10 s of tail and 0 latency,
/// AUDynamicsProcessor 0.2 s of tail and 256 samples of latency — so only a read
/// of the correct property satisfies all of them.
///
/// [`NO_TAIL_EFFECTS`] carries the zero-tail side of the same measurement.
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
/// The counterweight to [`TAIL_EFFECTS`]: a unit that genuinely has no tail
/// answers the property with `0.0`, which is a different fact from an
/// instrument's refusal to answer at all. Keeping a named zero-tail unit is what
/// stops a host from "helpfully" absorbing the refusal into a zero — the two
/// would then be indistinguishable, and a bounce would truncate the tail of
/// every unit whose tail it could not read.
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
/// Measured on macOS 15.6 by setting a mono float32 ASBD on the output (and,
/// where present, input) scope and reading it back: these report
/// `mChannelsPerFrame == 1` and initialize with `noErr`. 12 of the 15 units
/// probed did; the exceptions are in [`REFUSES_MONO`].
///
/// Named rather than derived at runtime because the point is to pin a *measured*
/// fact: a host that silently widened mono to stereo would still pass a test that
/// asked the AU what it supports and then asserted agreement with itself.
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
/// The count and the end labels are pinned, not merely "non-empty", because the
/// failure being guarded is a **truncated or misordered** `CFArray` walk — an
/// off-by-one in the index loop, or elements silently dropped — and every one of
/// those passes an "at least one string" check.
///
/// Measured on macOS 15.6. AUNBandEQ publishes the same 11 filter names on each
/// of its 8 band `Type` parameters (ids 2000..=2007); id 2000 is the
/// representative. Note it does this with
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
/// Note the distinction this count draws, which is not obvious and which cost a
/// wrong assertion to find: AUDistortion **names** 7 clumps (1..=7, verified in
/// `distortion_names_its_seven_sections`) but only 6 of them are claimed by a
/// parameter — nothing carries clump 6 ("Filter"). So "clumps the AU can name"
/// and "clumps the AU actually uses" are different sets, and a UI built from the
/// parameter list will render 6 sections while the AU can label 7. This constant
/// is the *claimed* count, because that is what a section list is built from.
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
/// **1. It is the only corpus unit macOS ships real `.aupreset` *files* for.**
/// 55 Apple-authored files under `/System/Library/Audio/Tunings/**/AU/` carry
/// `type`/`subtype`/`manufacturer` = `aumx`/`3dem`/`appl`. That makes it the
/// crate's only **interoperability** subject: every other preset assertion
/// round-trips a file this host wrote, which proves self-consistency and would
/// pass even if this host and Logic disagreed about the format. Loading Apple's
/// own file proves the format itself is right. See [`APPLE_PRESET_DIRS`].
///
/// **2. It is the only Apple unit advertising `kAudioUnitParameterFlag_CanRamp`,
/// and the proof that flag is a claim rather than a guarantee.** Surveying every
/// unit that initializes: 155 of 486 parameters across 45 units carry `CanRamp`,
/// but 145 are third-party (TDR Nova 37/75, TAL Reverb 4 20/20, TAL-NoiseMaker
/// 88/88); AUSpatialMixer is the entire Apple contribution at 10 of 12. And it
/// does **not** honour a ramp: scheduling one across `global reverb gain`
/// (id 9, range -40..40) versus pinning at the ramp's start value produces
/// envelopes differing by exactly `0.000000000` over 5 runs, while the readback
/// *does* land on the ramp's end value. The endpoint is applied and the
/// interpolation discarded — a step at the block boundary, the zipper artifact
/// ramping exists to avoid. That negative result is what stops a host from
/// trusting `can_ramp`. See
/// `au_render_notify.rs::the_can_ramp_flag_is_a_claim_not_a_guarantee`.
pub const SPATIAL_MIXER: AuRef = AuRef::mixer("AUSpatialMixer", b"3dem");

/// Directories macOS ships Apple-authored `.aupreset` files in.
///
/// Searched in order and treated as a set rather than a single hardcoded path
/// because the layout is an OS implementation detail: the `Generic/AU` folder
/// holds the device-independent presets while the per-tuning `AID*/AU` folders
/// hold hardware-specific ones, and which exist varies with the OS build and the
/// audio hardware attached. A test wants *any* genuine Apple preset for a corpus
/// unit, so it scans.
///
/// Note this is deliberately NOT `/Library/Audio/Presets/`, which the AU
/// documentation names as the user/third-party preset location: that directory
/// does **not exist** on this machine (measured — no `.aupreset` file anywhere
/// under `/Library/Audio` or `~/Library/Audio`), because Apple's units ship their
/// presets as in-bundle factory presets rather than as loose files. The Tunings
/// tree is where loose Apple `.aupreset` files actually are.
pub const APPLE_PRESET_DIRS: &[&str] = &[
    "/System/Library/Audio/Tunings/Generic/AU",
    "/System/Library/Audio/Tunings",
];

/// Locate a genuine Apple-authored `.aupreset` file belonging to `unit`.
///
/// Walks [`APPLE_PRESET_DIRS`] recursively and returns the first file whose
/// **identity keys** name `unit` — matched by parsing the preset, never by its
/// filename. Filenames happen to embed the codes today
/// (`aumx-3dem-appl-headphone-general-stereo.aupreset`), but that is a convention
/// of Apple's build scripts, not part of the format, and the same
/// name-versus-codes rule the module docs state for AU lookup applies here.
///
/// Returns `None` when the OS ships no preset for that unit. Unlike a missing
/// *AU*, that is not an environment failure: these files are an implementation
/// detail of Apple's spatial-audio tuning system, not a documented part of macOS,
/// so a caller reports the absence rather than asserting against it.
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
/// **This is the one exception to the hard-failure rule** in this module's docs,
/// and the exception is principled rather than convenient: the rule exists
/// because Apple's units *ship with macOS*, so their absence means a broken
/// environment. A third-party plugin is a genuine optional install — asserting
/// its presence would make the suite fail on any machine that simply does not
/// have it, which is a false alarm rather than a caught regression.
///
/// The discipline that keeps this from becoming the silent skip the module docs
/// warn about: every caller must assert something **unconditional** as well, so
/// the test still proves a property when the optional unit is missing. See
/// `au_render_notify.rs::a_ramp_is_honoured_where_a_plugin_implements_it`, which
/// pins the Apple negative result whether or not the third-party unit is found,
/// and prints a loud notice when it is not.
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
/// **Only the instruments.** Every Apple effect and mixer on the system refuses
/// the property with `kAudioUnitErr_InvalidProperty` (-10879) — for the read, the
/// write, and `GetPropertyInfo` alike. That is a surprising enough shape that it
/// is worth being explicit: the property whose entire purpose is "this is a
/// bounce, take the slow path" is not implemented by any of the units that would
/// most obviously use it.
///
/// The counterweight is [`WITHOUT_OFFLINE_RENDER`]. Both lists matter: without a
/// named refusing unit, the host's decision to propagate the refusal as an error
/// rather than flatten it to `false` could not be pinned.
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
/// And it accepts exactly one input buffer list: a second is refused with
/// `kAudioUnitErr_InvalidElement` (-10877), which is correct, since AUReverb2 has
/// one input element. Every other unit — including AUMultiChannelMixer, which has
/// **8** real input elements — answers `unimpErr` for 1, 2 and 8 lists alike, so
/// the absence is the selector rather than the topology.
///
/// The consequence, recorded here because it is the headline finding: **there is
/// no working AU sidechain on this machine.** `AudioUnitProcessMultiple` is the
/// only AUv2 call that can carry a second input bus, and nothing implements it in
/// a form that accepts one.
pub const IMPLEMENTS_PROCESS_MULTIPLE: AuRef = REVERB2;

/// `unimpErr` — the status an AU returns for a dispatch selector it does not
/// implement. Re-exported from the host crate rather than re-spelled so the two
/// cannot drift.
pub const UNIMP_ERR: i32 = tutti_au_host::types::UNIMP_ERR;

/// `kAudioUnitErr_TooManyFramesToProcess`, the AU's own answer to a render wider
/// than the `MaximumFramesPerSlice` it was initialized at.
///
/// Pinned so the host's `InvalidBuffer` guard can be shown to fire *instead of*
/// this rather than merely *alongside* it: the host must refuse the oversized
/// render before handing the AU a buffer list whose `mDataByteSize` overstates
/// storage the host actually allocated.
pub const TOO_MANY_FRAMES: i32 = -10874;

/// A 440 Hz sine at `amplitude`, `channels` wide, starting at sample `offset`.
///
/// Deliberately not silence: an in-place render path can be wrong in ways silence
/// cannot reveal — reading its own output as input, or emitting the previous block
/// — and every such failure still produces zeroes when fed zeroes.
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
