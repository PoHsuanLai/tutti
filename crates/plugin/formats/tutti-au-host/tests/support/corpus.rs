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
