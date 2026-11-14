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
            main_thread(),
            &tutti_fixtures::detached_work(),
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
              VST2's audioMasterGetTime, AU's host callbacks) and has no event for a change inside it",
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
    const GAPS: &'static [(&'static str, Gap)] = &[NO_TRANSPORT_EVENT];

    fn load() -> Self::L {
        tutti_vst2_host::Vst2Loaded::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            plugin::library(),
        )
        .expect("the plugin loads")
    }
}

fn vst3(module: std::path::PathBuf) -> tutti_vst3_host::Vst3Loaded {
    tutti_vst3_host::Vst3Loaded::load(
        main_thread(),
        &tutti_fixtures::detached_work(),
        module,
        tutti_vst3_host::LoadOptions::new(),
    )
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

/// AU, from the shared plugin's Audio Unit shell, registered in this process
/// (`tutti_shared_plugin::register_au`) and found by the host through a bundle
/// that declares it (`plugin::au_bundle`). The unit exists in this process
/// only, so there is no served (out-of-process) run of it.
#[cfg(target_os = "macos")]
pub struct Au;

#[cfg(target_os = "macos")]
impl Format for Au {
    const NAME: &'static str = "au";
    const SURFACE: spec::Surface = spec::AU;
    type L = tutti_au_host::AuLoaded;
    /// The host schedules each segment between two points as a ramped event and
    /// a step as an immediate one (`AudioUnitScheduleParameters`, from the
    /// pre-render notify). The unit's
    /// listeners are told at the control tick by `AUParameterListenerNotify`,
    /// which writes nothing, so the unit sees no further `SetParameter`.
    const PARAM_TIMING: ParamTiming = ParamTiming::Ramped;
    /// An AU holds a parameter as an `f32`.
    const PARAM_VALUE_TOLERANCE: f64 = f32::EPSILON as f64;
    const GAPS: &'static [(&'static str, Gap)] = &[
        NO_TRANSPORT_EVENT,
        (
            "a_gesture_is_reported_in_order",
            Gap {
                why: "AudioToolbox coalesces the value changes of one gesture that arrive within \
                      its listener's interval into the last, so a host hears a begin, one change \
                      and an end where the spec's gesture has two changes; the shell raises no \
                      notice of its own, and the host's reading of an editor's gesture is \
                      `tutti-au-host`'s `a_gesture_in_the_plugins_own_editor_is_reported_in_order`",
                fails_with: "the gesture, in order",
            },
        ),
        (
            "f64_carries_same_audio",
            Gap {
                why: "an AUv2 unit has no 64-bit render: the host narrows an f64 block to f32 \
                      and widens the answer, so 0.1 + 1.0 comes back as the f32 sum",
                fails_with: "ch0",
            },
        ),
    ];

    fn load() -> Self::L {
        tutti_au_host::AuLoaded::load(
            main_thread(),
            &tutti_fixtures::detached_work(),
            &plugin::au_bundle(),
            &tutti_plugin_types::Load::new().plugin(tutti_shared_plugin::AU_ID),
        )
        .expect("the shared AU loads")
    }
}
