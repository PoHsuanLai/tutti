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
/// A sampler with no input bus, driven by MIDI.
pub const SAMPLER: AuRef = AuRef::instrument("AUSampler", b"samp");
/// The built-in DLS synth: no input bus, driven by MIDI.
pub const DLS_SYNTH: AuRef = AuRef::instrument("DLSMusicDevice", b"dls ");

/// Every effect in the corpus, for tests that assert a property across all of
/// them rather than picking one representative.
pub const EFFECTS: &[AuRef] = &[DELAY, N_BAND_EQ, LOWPASS, DYNAMICS];

/// Every instrument in the corpus.
pub const INSTRUMENTS: &[AuRef] = &[SAMPLER, DLS_SYNTH];

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
