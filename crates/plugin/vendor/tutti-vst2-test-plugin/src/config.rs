//! Construction-time metadata, read from `TUTTI_VST2_PROBE_*` env vars.
//!
//! # Why env vars here and `extern "C"` switches elsewhere
//!
//! `vst::main` calls `Plugin::get_info()` *once*, synchronously, while
//! building the `AEffect` — before `VSTPluginMain` has even returned, so
//! before the test could possibly call a switch on the loaded image.
//! Anything that lands in the AEffect (`numInputs`, `numParams`,
//! `numPrograms`, `flags`, `initialDelay`) is therefore only reachable
//! through the process environment. This is the same split the CLAP probe
//! draws and the same one the VST3 probe's README documents: read-once
//! construction state is an env var, everything a running plugin can change
//! its mind about is an `extern "C"` switch (see `switches.rs`).
//!
//! A test setting these must do so before `Vst2Instance::load`, and must
//! serialize against other tests — `std::env::set_var` is process-global.

use std::env;

use vst::plugin::Category;

/// Parse a `TUTTI_VST2_PROBE_*` variable, falling back to `default` when
/// unset or unparseable. Silent fallback on garbage is deliberate: a typo
/// in a test's env var should produce the well-behaved probe (whose
/// assertions then fail loudly) rather than a panic inside `dlopen`, where
/// the host would report it as a load failure and hide the real cause.
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(default)
}

fn env_flag(name: &str) -> bool {
    matches!(env::var(name).as_deref(), Ok("1") | Ok("true"))
}

/// Everything the probe declares to the host at construction time.
///
/// Defaults describe the *well-behaved* probe: a stereo-in / stereo-out
/// effect with four parameters and two programs, no latency, no tail. Every
/// field is overridable so a test can build a host-hostile shape without a
/// second probe binary.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// `AEffect::numInputs`.
    pub inputs: i32,
    /// `AEffect::numOutputs`.
    pub outputs: i32,
    /// `AEffect::numParams`.
    pub parameters: i32,
    /// `AEffect::numPrograms`.
    pub programs: i32,
    /// Number of parameters the probe will actually service. When this is
    /// below `parameters`, the probe is advertising an enumeration hole —
    /// the classic out-of-bounds trigger, and the VST2 analogue of the
    /// `getBusCount` overreport that produced one of the VST3 bugs.
    pub serviced_parameters: i32,
    /// Number of programs the probe will actually name. Same hole, on the
    /// preset axis.
    pub serviced_programs: i32,
    /// `AEffect::initialDelay`.
    pub initial_delay: i32,
    /// Plugin category. `Synth` also sets `effFlagsIsSynth` via `vst::main`.
    pub category: Category,
    /// Sets `effFlagsProgramChunks`.
    pub preset_chunks: bool,
    /// Sets `effFlagsCanDoubleReplacing`.
    pub f64_precision: bool,
    /// Whether `get_editor` returns an editor, which is what makes
    /// `vst::main` set `effFlagsHasEditor`.
    pub has_editor: bool,
    /// Clear `effFlagsCanReplacing` from the AEffect after `vst::main` has
    /// built it. `vst::main` sets that bit unconditionally, so this cannot
    /// be expressed through the `Plugin` trait — see `lib.rs`'s
    /// `VSTPluginMain`.
    pub omit_can_replacing: bool,
    /// Raw `effGetTailSize` answer, bypassing the `Plugin` trait.
    ///
    /// vst-rs's dispatcher rewrites a trait-reported `0` into `1`, which
    /// erases exactly the distinction VST 2.4 draws: `0` means "no tail
    /// info, assume the worst / ask again", `1` means "no tail at all,
    /// safe to stop rendering immediately", and anything larger is a
    /// sample count. A host that conflates them either truncates reverb
    /// tails or renders silence forever. `None` leaves the trait path
    /// alone; `Some(n)` answers `n` verbatim from the raw dispatcher.
    pub raw_tail_size: Option<isize>,
    /// Number of MIDI input channels declared.
    pub midi_inputs: i32,
    /// Number of MIDI output channels declared.
    pub midi_outputs: i32,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            inputs: 2,
            outputs: 2,
            parameters: 4,
            programs: 2,
            serviced_parameters: 4,
            serviced_programs: 2,
            initial_delay: 0,
            category: Category::Effect,
            preset_chunks: true,
            f64_precision: false,
            has_editor: false,
            omit_can_replacing: false,
            raw_tail_size: None,
            midi_inputs: 1,
            midi_outputs: 1,
        }
    }
}

impl ProbeConfig {
    /// Read the config out of the environment. Called once per plugin
    /// instance, from `Plugin::new`.
    pub fn from_env() -> Self {
        let d = Self::default();
        let parameters = env_or("TUTTI_VST2_PROBE_PARAMS", d.parameters);
        let programs = env_or("TUTTI_VST2_PROBE_PROGRAMS", d.programs);
        Self {
            inputs: env_or("TUTTI_VST2_PROBE_INPUTS", d.inputs),
            outputs: env_or("TUTTI_VST2_PROBE_OUTPUTS", d.outputs),
            parameters,
            programs,
            // Default the serviced counts to the declared counts: absent an
            // explicit override the probe is honest, and a test that wants a
            // hole names its size.
            serviced_parameters: env_or("TUTTI_VST2_PROBE_SERVICED_PARAMS", parameters),
            serviced_programs: env_or("TUTTI_VST2_PROBE_SERVICED_PROGRAMS", programs),
            initial_delay: env_or("TUTTI_VST2_PROBE_LATENCY", d.initial_delay),
            category: if env_flag("TUTTI_VST2_PROBE_IS_SYNTH") {
                Category::Synth
            } else {
                d.category
            },
            preset_chunks: !env_flag("TUTTI_VST2_PROBE_NO_CHUNKS"),
            f64_precision: env_flag("TUTTI_VST2_PROBE_F64"),
            has_editor: env_flag("TUTTI_VST2_PROBE_EDITOR"),
            omit_can_replacing: env_flag("TUTTI_VST2_PROBE_NO_CAN_REPLACING"),
            raw_tail_size: env::var("TUTTI_VST2_PROBE_TAIL_SIZE")
                .ok()
                .and_then(|v| v.parse::<isize>().ok()),
            midi_inputs: env_or("TUTTI_VST2_PROBE_MIDI_INPUTS", d.midi_inputs),
            midi_outputs: env_or("TUTTI_VST2_PROBE_MIDI_OUTPUTS", d.midi_outputs),
        }
    }
}

/// Per-channel DC tag added to the passthrough signal, the routing oracle.
///
/// `out[ch][i] = in[ch][i] + tag(ch)`. The offsets are distinct and not
/// multiples of each other, so a host that swaps two channels, duplicates
/// one across both, or wires an input onto the wrong output slot produces
/// arithmetically wrong samples that no amount of "is it finite / is it
/// non-silent" checking would catch. 100.0 spacing keeps the tag far above
/// any plausible audio content, so an assertion failure names the channel
/// that went wrong by inspection.
pub const fn channel_tag(channel: usize) -> f32 {
    // Chosen over `ch as f32` so an off-by-one in the host's channel loop
    // cannot be mistaken for rounding.
    (channel as f32) * 100.0 + 1.0
}

#[cfg(test)]
mod tests {
    use super::channel_tag;

    /// Pin the first few tags as literals.
    ///
    /// `channel_tag` is shared by the probe and the host test, so an
    /// assertion written as `in + channel_tag(ch)` moves with any change to
    /// this function and can never fail — it would report coverage that does
    /// not exist. The literals here are the anchor: change the formula and
    /// this test fails, which is the signal to go re-check every mirrored
    /// constant in `tutti-vst2-host`'s tests.
    #[test]
    fn tags_are_pinned_and_distinct() {
        assert_eq!(channel_tag(0), 1.0);
        assert_eq!(channel_tag(1), 101.0);
        assert_eq!(channel_tag(2), 201.0);
        // The gap must dwarf any plausible audio sample, so a swapped
        // channel cannot be mistaken for a loud one.
        assert!(channel_tag(1) - channel_tag(0) > 10.0);
    }
}
