//! One [`Format`] per host: the whole of a format's side of the contract.

use crate::active::main_thread;
use crate::{Format, Gap, ParamTiming, plugin, spec};

/// CLAP.
pub struct Clap;

impl Format for Clap {
    const NAME: &'static str = "clap";
    const SURFACE: spec::Surface = spec::CLAP;
    type L = tutti_clap_host::ClapLoaded;
    const PARAM_TIMING: ParamTiming = ParamTiming::SampleAccurate;
    const GAPS: &'static [(&'static str, Gap)] = &[];

    fn load() -> Self::L {
        tutti_clap_host::ClapLoaded::load(
            &main_thread(),
            plugin::clap(),
            tutti_clap_host::LoadOptions::new(),
        )
        .expect("the plugin loads")
    }
}

/// The format has no event that moves the transport inside a block: a
/// plugin reads one transport per block, at its first frame.
const NO_TRANSPORT_EVENT: (&str, Gap) = (
    "transport_change_is_told_at_its_frame",
    Gap {
        why: "the format carries one transport per block (VST3's ProcessContext, \
              VST2's audioMasterGetTime) and has no event for a change inside it",
        fails_with: "no transport at frame 32",
    },
);

/// VST2.
pub struct Vst2;

impl Format for Vst2 {
    const NAME: &'static str = "vst2";
    const SURFACE: spec::Surface = spec::VST2;
    type L = tutti_vst2_host::Vst2Loaded;
    /// VST2 has no sample offset on a parameter: the host's changes land at the
    /// block head.
    const PARAM_TIMING: ParamTiming = ParamTiming::BlockHead;
    const GAPS: &'static [(&'static str, Gap)] = &[
        (
            "reset_clears_the_delay_line",
            Gap {
                why: "VST 2.4 has no opcode that clears a plugin's DSP state; \
                      `Processor::reset` drops only the host's SysEx reassembly",
                fails_with: "the impulse came out after the reset",
            },
        ),
        NO_TRANSPORT_EVENT,
    ];

    fn load() -> Self::L {
        tutti_vst2_host::Vst2Loaded::load(&main_thread(), plugin::library())
            .expect("the plugin loads")
    }
}

fn vst3(module: std::path::PathBuf) -> tutti_vst3_host::Vst3Loaded {
    tutti_vst3_host::Vst3Loaded::load(&main_thread(), module, tutti_vst3_host::LoadOptions::new())
        .expect("the plugin loads")
}

/// VST3, from the Rust plugin: one object that is both the component and the
/// controller.
pub struct Vst3;

impl Format for Vst3 {
    const NAME: &'static str = "vst3";
    const SURFACE: spec::Surface = spec::VST3;
    type L = tutti_vst3_host::Vst3Loaded;
    const PARAM_TIMING: ParamTiming = ParamTiming::SampleAccurate;
    const GAPS: &'static [(&'static str, Gap)] = &[NO_TRANSPORT_EVENT];

    fn load() -> Self::L {
        vst3(plugin::vst3_module())
    }
}

/// VST3, from the C++ plugin on the Steinberg SDK classes: a separate
/// component and controller, and none of the `vst3` crate's bindings the host
/// shares with the Rust plugin.
pub struct Vst3Cpp;

impl Format for Vst3Cpp {
    const NAME: &'static str = "vst3-cpp";
    const SURFACE: spec::Surface = spec::VST3_CPP;
    type L = tutti_vst3_host::Vst3Loaded;
    const PARAM_TIMING: ParamTiming = ParamTiming::SampleAccurate;
    const GAPS: &'static [(&'static str, Gap)] = &[NO_TRANSPORT_EVENT];

    fn load() -> Self::L {
        vst3(plugin::vst3_cpp_module())
    }
}

/// A case that configures or reads the shared plugin through its state blob (a
/// mode word, the transcript): the unit under test has none, so the case stops
/// at the first read of it.
#[cfg(target_os = "macos")]
const NEEDS_THE_BLOB: Gap = Gap {
    why: "the case selects a mode or reads the transcript through the shared plugin's \
          state blob; AUDelay's state is an AU class-info property list with no such blob",
    fails_with: "the state holds the plugin's blob",
};

/// AU, from Apple's AUDelay in `CoreAudio.component`. There is no AU build of
/// the shared plugin, so this runs every case against a unit that has none of
/// its mode word, transcript or spec figures: a case that needs them is a gap.
#[cfg(target_os = "macos")]
pub struct Au;

#[cfg(target_os = "macos")]
impl Format for Au {
    const NAME: &'static str = "au";
    const SURFACE: spec::Surface = spec::AU;
    type L = tutti_au_host::AuLoaded;
    /// The host writes the last point of each parameter's queue at the block
    /// head (`AudioUnitSetParameter` with no ramp offset).
    const PARAM_TIMING: ParamTiming = ParamTiming::BlockHead;
    const GAPS: &'static [(&'static str, Gap)] = &[
        ("routing_tag_oracle", NEEDS_THE_BLOB),
        ("reconfigure_keeps_rendering", NEEDS_THE_BLOB),
        ("buses_reported", NEEDS_THE_BLOB),
        ("latency_reported", NEEDS_THE_BLOB),
        ("latency_applied", NEEDS_THE_BLOB),
        ("reset_clears_the_delay_line", NEEDS_THE_BLOB),
        ("tail_reported", NEEDS_THE_BLOB),
        ("active_write_applied", NEEDS_THE_BLOB),
        ("automation_point_offset", NEEDS_THE_BLOB),
        ("note_at_offset", NEEDS_THE_BLOB),
        ("arrival_is_ordered_and_sized", NEEDS_THE_BLOB),
        ("transport_reaches_the_plugin", NEEDS_THE_BLOB),
        ("transport_change_is_told_at_its_frame", NEEDS_THE_BLOB),
        ("loaded_write_reaches_the_audio", NEEDS_THE_BLOB),
        ("loaded_write_survives_a_save", NEEDS_THE_BLOB),
        ("reopen_is_a_fresh_instance", NEEDS_THE_BLOB),
        ("corrupt_state_leaves_plugin_usable", NEEDS_THE_BLOB),
        ("f64_carries_same_audio", NEEDS_THE_BLOB),
        ("overflowing_log_fails_the_oracle", NEEDS_THE_BLOB),
        ("process_does_not_allocate", NEEDS_THE_BLOB),
        (
            "parameters_listed",
            Gap {
                why: "AUDelay lists its own four parameters; the spec's plugin lists three",
                fails_with: "the listed parameters",
            },
        ),
        (
            "parameter_round_trip",
            Gap {
                why: "an AU holds a parameter as an f32 in plain units, so a normalized 0.75 \
                      reads back as 0.7500000169765675; the case asserts exact equality",
                fails_with: "0.7500000169765675",
            },
        ),
        (
            "state_round_trip",
            Gap {
                why: "an AU holds a parameter as an f32 in plain units, so a restored \
                      normalized 0.5 reads back as 0.4999999743455097; the case asserts \
                      exact equality",
                fails_with: "0.4999999743455097",
            },
        ),
    ];

    fn load() -> Self::L {
        tutti_au_host::AuLoaded::load(
            &main_thread(),
            std::path::Path::new("/System/Library/Components/CoreAudio.component"),
            &tutti_plugin_types::Load::new().plugin("au.aufx.dely.appl"),
        )
        .expect("AUDelay loads")
    }
}
