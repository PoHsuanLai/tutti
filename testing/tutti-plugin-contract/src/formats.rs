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
