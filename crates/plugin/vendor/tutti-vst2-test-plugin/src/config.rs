//! Construction-time metadata, read from `TUTTI_VST2_PROBE_*` env vars.
//!
//! Env vars rather than the `extern "C"` switches in `switches.rs` because
//! `vst::main` calls `get_info()` synchronously while building the `AEffect`,
//! before `VSTPluginMain` returns — so anything landing in the AEffect
//! (`numInputs`, `numParams`, `numPrograms`, `flags`, `initialDelay`) is only
//! reachable through the process environment.
//!
//! A test must set these before `Vst2Instance::load` and serialize against
//! other tests — `set_var` is process-global.

use std::env;

use vst::plugin::Category;

/// Parse a `TUTTI_VST2_PROBE_*` variable, falling back to `default` when
/// unset or unparseable. Silent fallback on garbage is deliberate: a typo
/// should yield the well-behaved probe, whose assertions then fail loudly,
/// rather than a panic inside `dlopen` that the host reports as a load
/// failure and that hides the real cause.
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
/// effect, four parameters, two programs, no latency, no tail.
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
    /// Number of parameters the probe will actually service. Below
    /// `parameters`, it is advertising an enumeration hole — a host that
    /// walks `0..numParams` and trusts every answer reads past the end.
    pub serviced_parameters: i32,
    /// Number of programs the probe will actually name. The same hole, on
    /// the preset axis.
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
    /// Clear `effFlagsCanReplacing` after `vst::main` has built the AEffect.
    /// `vst::main` sets that bit unconditionally, so it cannot be expressed
    /// through the `Plugin` trait — see `lib.rs`'s `VSTPluginMain`.
    pub omit_can_replacing: bool,
    /// Raw `effGetTailSize` answer, bypassing the `Plugin` trait, whose path
    /// vst-rs rewrites from `0` to `1` — erasing VST 2.4's distinction
    /// between `0` ("no tail info, assume the worst") and `1` ("no tail,
    /// safe to stop rendering"); larger values are sample counts. `None`
    /// leaves the trait path alone; `Some(n)` answers `n` verbatim.
    pub raw_tail_size: Option<isize>,
    /// Number of MIDI input channels declared.
    pub midi_inputs: i32,
    /// Number of MIDI output channels declared.
    pub midi_outputs: i32,
    /// What the probe answers to `effGetEffectName` (45).
    ///
    /// `None` declines the opcode, which is the default and the common case
    /// among real plugins. `Some` makes the probe answer a name deliberately
    /// different from `Info::name` (`effGetProductString`), so a test can tell
    /// which of the two the host read.
    pub effect_name: Option<String>,
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
            effect_name: None,
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
            // Default the serviced counts to the declared ones, so the probe
            // is honest unless a test names a hole size explicitly.
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
            effect_name: env::var("TUTTI_VST2_PROBE_EFFECT_NAME").ok(),
        }
    }
}

/// Per-channel DC tag added to the passthrough signal — the routing oracle.
///
/// `out[ch][i] = in[ch][i] + tag(ch)`. The offsets are distinct and not
/// multiples of each other, so a host that swaps two channels, duplicates one
/// across both, or wires an input onto the wrong output slot produces
/// arithmetically wrong samples that "is it finite / non-silent" checking
/// cannot catch. The 100.0 spacing puts the tag far above plausible audio, so
/// a failure names the guilty channel by inspection.
pub const fn channel_tag(channel: usize) -> f32 {
    // Not `ch as f32`: an off-by-one in the host's channel loop must not be
    // mistakable for rounding.
    (channel as f32) * 100.0 + 1.0
}

#[cfg(test)]
mod tests {
    use super::channel_tag;

    /// Pin the tags as literals. `channel_tag` is shared by the probe and the
    /// host tests, so an expectation written as `in + channel_tag(ch)` moves
    /// with the function and can never fail. These literals are the anchor:
    /// a failure here is the signal to re-check every mirrored constant in
    /// `tutti-vst2-host`'s tests.
    #[test]
    fn tags_are_pinned_and_distinct() {
        assert_eq!(channel_tag(0), 1.0);
        assert_eq!(channel_tag(1), 101.0);
        assert_eq!(channel_tag(2), 201.0);
        // The gap must dwarf any plausible sample, so a swapped channel
        // cannot be mistaken for a loud one.
        assert!(channel_tag(1) - channel_tag(0) > 10.0);
    }
}
