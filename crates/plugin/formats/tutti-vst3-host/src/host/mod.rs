//! Plugin lifecycle types: [`Vst3Library`] (loaded DSO + factory),
//! [`Vst3Loaded`] (initialized plugin, no audio), and the two halves an
//! activation splits it into, [`Vst3Processor`] (renders blocks) and
//! [`Vst3Controller`] (everything else). The stages are distinct types and the
//! transitions consume their inputs, so the compiler enforces the ordering.
//!
//! The types are one state machine split across files, and the split follows
//! the transitions: `library` builds a [`Vst3Loaded`] from a factory, `loaded`
//! owns the pre-activation surface, `control` its shared-vocabulary face,
//! `instance` the activation sequence (`setBusArrangements` to `setActive`),
//! `controller` the edges between `Loaded` and the pair, and `processor` the
//! audio path.
//!
//! [`Vst3Loaded`] is the state most of this module's code is about: it holds
//! the COM interfaces, the host context and the editor, and it survives an
//! activate/deactivate round trip intact (the Controller holds it and `Deref`s
//! to it).

#[cfg(feature = "conformance")]
pub mod conformance;
mod control;
mod controller;
mod instance;
mod library;
mod loaded;
mod midi_learn;
mod midi_mapping;
mod processor;
mod sequencer;
mod shell;
mod speakers;

pub use control::descriptor_of;
pub use controller::Vst3Controller;
pub use library::{ClassInfo, FactoryInfo, Vst3Library, factory_flags};
pub use loaded::{LoadOptions, RestartOutcome, Vst3Loaded};
pub use processor::{ProcessFailed, Vst3Processor, Vst3ProcessorAny};

/// Editor-lifecycle internals, exposed for the conformance tests. Both are
/// reached in production only through `open_editor`/`close_editor`, which need
/// a real plugin and a display; the spec rules they encode — the teardown call
/// *order*, and which `isPlatformTypeSupported` results count as a refusal —
/// are worth pinning without either.
#[cfg(feature = "conformance")]
pub use tutti_vst3_ffi::{
    detach_view, platform_type_refused, send_key_down, send_key_up, send_wheel, set_view_focus,
};

// ── Bus activation policy ─────────────────────────────────────────────────────

use vst3::Steinberg::Vst::{BusInfo_::BusFlags_, BusTypes_::kMain};

use crate::helpers::sdk_enum_i32;

/// Whether this host activates a bus at load, given its `busType` and `flags`.
///
/// **`kMain` unconditionally; `kAux` only when it asks.** The two halves are
/// decided by different arguments.
///
/// A main bus is the plugin's actual signal path, so a host that skipped one
/// would render silence. The flag is not trustworthy enough to gate that:
/// `ivstcomponent.h:54-55` calls it *"only a wish, the host is allow to not
/// follow it"*, and a plugin that simply forgot to set it on its main output
/// is a real and unremarkable bug. Steinberg's own VST2/AU wrapper reaches the
/// same conclusion from the other direction — `basewrapper.cpp:1061-1085`
/// activates every `kMain` without consulting the flag at all.
///
/// An aux bus is where the flag carries information. Sidechains and extra stem
/// outputs are exactly what a plugin declares but does not expect fed by
/// default, and host-checker's ten unflagged aux inputs are the shape: a host
/// that activates them all must then stage ten silent buses every block.
///
/// This is a preference, not conformance. Activating everything was legal —
/// the SDK's own validator *requires* an unflagged bus to be activatable
/// (`busactivation.cpp:66-71` fails a plugin that refuses). What this policy
/// does is stop overriding a signal the plugin took the trouble to send.
///
/// # Why not the strict reading
///
/// Honouring the flag on main buses too is what `auwrapper.mm:553` does behind
/// `SMTG_AUWRAPPER_ACTIVATE_ONLY_DEFAULT_ACTIVE_BUSES`, and its own CMake note
/// says why it is off by default: *"This may not work on some hosts because
/// they never activate a bus later."* That is this host today — there is no
/// public per-bus activation API, so a bus skipped here is unreachable for the
/// lifetime of the instance rather than merely inactive.
pub(super) fn wants_activation(bus_type: i32, flags: u32) -> bool {
    bus_type == K_MAIN || (flags & BUS_DEFAULT_ACTIVE) != 0
}

const K_MAIN: i32 = sdk_enum_i32(kMain);

/// `BusFlags` is `DefaultEnumType`, which is `u32` on unix and `c_int` on
/// Windows — so the cast is a no-op here and load-bearing there. Same reason
/// [`crate::physical_ui_type`] carries this allow.
#[allow(clippy::unnecessary_cast)]
const BUS_DEFAULT_ACTIVE: u32 = BusFlags_::kDefaultActive as u32;

#[cfg(test)]
mod bus_activation_policy_tests {
    use super::{BUS_DEFAULT_ACTIVE, K_MAIN, wants_activation};

    /// `kAux` is not zero, so a test using `0` for "not main" would be testing
    /// the main path by accident.
    const K_AUX: i32 = crate::helpers::sdk_enum_i32(vst3::Steinberg::Vst::BusTypes_::kAux);

    /// A main bus is activated whether or not it carries the flag.
    ///
    /// The unflagged case is the one that matters: a plugin that forgot
    /// `kDefaultActive` on its main output would otherwise render silence, and
    /// the host cannot tell that apart from a deliberate omission.
    #[test]
    fn a_main_bus_is_activated_regardless_of_the_flag() {
        assert!(wants_activation(K_MAIN, BUS_DEFAULT_ACTIVE));
        assert!(wants_activation(K_MAIN, 0));
    }

    /// An aux bus is activated only when it asks to be.
    #[test]
    fn an_aux_bus_is_activated_only_when_it_asks() {
        assert!(wants_activation(K_AUX, BUS_DEFAULT_ACTIVE));
        assert!(!wants_activation(K_AUX, 0));
    }

    /// The flag is read as one bit among several, not as the whole field.
    ///
    /// `kIsControlVoltage` and friends share `flags`, so an equality test
    /// against `kDefaultActive` would drop every bus that sets both — and a CV
    /// sidechain that asked to be active is exactly such a bus.
    #[test]
    fn other_bus_flags_do_not_mask_the_default_active_bit() {
        #[allow(clippy::unnecessary_cast)] // platform-varying; see BUS_DEFAULT_ACTIVE
        const K_IS_CONTROL_VOLTAGE: u32 =
            vst3::Steinberg::Vst::BusInfo_::BusFlags_::kIsControlVoltage as u32;

        assert!(wants_activation(
            K_AUX,
            BUS_DEFAULT_ACTIVE | K_IS_CONTROL_VOLTAGE
        ));
        assert!(!wants_activation(K_AUX, K_IS_CONTROL_VOLTAGE));
    }

    /// `kDefaultActive` is bit 0. Pinned because the policy reads it by mask:
    /// were it ever a different bit, every assertion above would still pass
    /// while the mask silently matched the wrong flag.
    #[test]
    fn the_default_active_bit_is_the_one_the_sdk_defines() {
        assert_eq!(BUS_DEFAULT_ACTIVE, 1);
        assert_eq!(K_MAIN, 0);
    }
}
