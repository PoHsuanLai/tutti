//! Plugin lifecycle types: [`Vst3Library`] (loaded DSO + factory),
//! [`Vst3Loaded`] (initialized plugin, no audio), and [`Vst3Instance`] (active,
//! ready to `process`). Stages are encoded as distinct types — transitions
//! consume `self` so the compiler enforces the ordering.

mod bus_buffers;
#[cfg(feature = "conformance")]
pub mod conformance;
mod instance;
mod library;
mod loaded;
mod midi_learn;
mod midi_mapping;
mod module_entry;
mod plugin_state;

pub use instance::Vst3Instance;
pub use library::{ClassInfo, Vst3Library};
pub use loaded::{PluginNotifications, RestartOutcome, Vst3Loaded};

/// Editor-lifecycle internals, exposed for the conformance tests. Both are
/// reached in production only through `open_editor`/`close_editor`, which need
/// a real plugin and a display; the spec rules they encode — the teardown call
/// *order*, and which `isPlatformTypeSupported` results count as a refusal —
/// are worth pinning without either.
#[cfg(feature = "conformance")]
pub use loaded::{detach_view, platform_type_refused};

// ── IComponent extension trait ────────────────────────────────────────────────

use vst3::ComPtr;
use vst3::Steinberg::{
    kResultOk,
    Vst::{
        BusDirections_::{kInput, kOutput},
        BusInfo_::BusFlags_,
        BusTypes_::kMain,
        IComponent, IComponentTrait,
        MediaTypes_::kAudio,
    },
};

use tutti_types::ChannelLayout;

use crate::types::BusInfo as BusInfoWrap;

pub(super) const K_AUDIO: i32 = kAudio as i32;
pub(super) const K_INPUT: i32 = kInput as i32;
pub(super) const K_OUTPUT: i32 = kOutput as i32;

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
/// (`busactivation.cpp:66-71` fails a plugin that refuses), so nothing here
/// fixes a defect. What it does is stop overriding a signal the plugin took
/// the trouble to send.
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

const K_MAIN: i32 = kMain as i32;

/// `BusFlags` is `DefaultEnumType`, which is `u32` on unix and `c_int` on
/// Windows — so the cast is a no-op here and load-bearing there. Same reason
/// [`crate::physical_ui_type`] carries this allow.
#[allow(clippy::unnecessary_cast)]
const BUS_DEFAULT_ACTIVE: u32 = BusFlags_::kDefaultActive as u32;

pub(super) trait IComponentExt {
    /// Channel layout of **bus 0** in `direction`, exactly as the plugin
    /// reports it.
    ///
    /// `None` when the plugin reports no buses or `getBusInfo` fails — the two
    /// cases where there is no answer to carry. A plugin that answers with zero
    /// channels is reported as zero: that is a bus the plugin declared and then
    /// said is empty, which is a different fact from "no bus" and is not this
    /// function's to reconcile.
    ///
    /// A negative `channelCount` is floored to 0. The ABI types it `int32` with
    /// no negative meaning, so this is the sole out-of-contract clamp here, not
    /// a policy about empty buses.
    fn audio_bus_channel_count(&self, direction: i32) -> Option<ChannelLayout>;

    /// Channel count of every audio bus in `direction`, in bus-index order.
    /// A bus whose query fails contributes 0 so the vec length always equals
    /// the bus count.
    fn audio_bus_channels(&self, direction: i32) -> Vec<usize>;
}

impl IComponentExt for ComPtr<IComponent> {
    fn audio_bus_channel_count(&self, direction: i32) -> Option<ChannelLayout> {
        unsafe {
            if self.getBusCount(K_AUDIO, direction) <= 0 {
                return None;
            }
            let mut bus = BusInfoWrap::default();
            if self.getBusInfo(K_AUDIO, direction, 0, bus.as_mut_inner()) == kResultOk {
                Some(ChannelLayout::from(bus.channel_count().max(0) as u16))
            } else {
                None
            }
        }
    }

    fn audio_bus_channels(&self, direction: i32) -> Vec<usize> {
        unsafe {
            let num_buses = self.getBusCount(K_AUDIO, direction);
            if num_buses <= 0 {
                return Vec::new();
            }
            (0..num_buses)
                .map(|i| {
                    let mut bus = BusInfoWrap::default();
                    if self.getBusInfo(K_AUDIO, direction, i, bus.as_mut_inner()) == kResultOk {
                        bus.channel_count().max(0) as usize
                    } else {
                        0
                    }
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod bus_activation_policy_tests {
    use super::{wants_activation, BUS_DEFAULT_ACTIVE, K_MAIN};

    /// `kAux` is not zero, so a test using `0` for "not main" would be testing
    /// the main path by accident.
    const K_AUX: i32 = vst3::Steinberg::Vst::BusTypes_::kAux as i32;

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
