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
        IComponent, IComponentTrait,
        MediaTypes_::kAudio,
    },
};

use tutti_types::ChannelLayout;

use crate::types::BusInfo as BusInfoWrap;

pub(super) const K_AUDIO: i32 = kAudio as i32;
pub(super) const K_INPUT: i32 = kInput as i32;
pub(super) const K_OUTPUT: i32 = kOutput as i32;

pub(super) trait IComponentExt {
    /// Channel layout of the first audio bus in `direction`.
    /// Returns `None` when the plugin reports no buses or the query fails.
    /// `min_channels` clamps the result upward (e.g. 1 for outputs).
    fn audio_bus_channel_count(&self, direction: i32, min_channels: i32) -> Option<ChannelLayout>;

    /// Channel count of every audio bus in `direction`, in bus-index order.
    /// A bus whose query fails contributes 0 so the vec length always equals
    /// the bus count.
    fn audio_bus_channels(&self, direction: i32) -> Vec<usize>;
}

impl IComponentExt for ComPtr<IComponent> {
    fn audio_bus_channel_count(&self, direction: i32, min_channels: i32) -> Option<ChannelLayout> {
        unsafe {
            if self.getBusCount(K_AUDIO, direction) <= 0 {
                return None;
            }
            let mut bus = BusInfoWrap::default();
            if self.getBusInfo(K_AUDIO, direction, 0, bus.as_mut_inner()) == kResultOk {
                Some(ChannelLayout::from(
                    bus.channel_count().max(min_channels) as u16
                ))
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
