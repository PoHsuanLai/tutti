//! VST3-specific sample-format extension on top of `tutti_plugin_types::Sample`.

use vst3::Steinberg::Vst::{AudioBusBuffers, ProcessModes_, SymbolicSampleSizes_};

pub(crate) const K_SAMPLE_32_INT: i32 = SymbolicSampleSizes_::kSample32 as i32;
pub(crate) const K_SAMPLE_64_INT: i32 = SymbolicSampleSizes_::kSample64 as i32;

/// Which of VST3's three processing modes (`ProcessModes_`) the host is asking
/// for: live playback, offline bounce, or the look-ahead prefetch mode.
///
/// The distinction is not cosmetic. A plugin is entitled to behave differently
/// per mode — lookahead limiters may use their full lookahead rather than a
/// latency-bounded approximation, resamplers may switch to a higher-quality
/// kernel, and FFT-based processors may use longer windows — because offline
/// rendering has no deadline. A host that never asks for anything but
/// [`Realtime`](Self::Realtime) silently gets the realtime result from an
/// offline bounce.
///
/// # Where this value must appear
///
/// VST3 carries the mode in two places that the spec requires to agree:
/// `ProcessSetup::processMode` (sent once, at `setupProcessing`) and
/// `ProcessData::processMode` (sent every block). HostChecker's
/// `ProcessSetupCheck::check` reports `kLogIdInvalidProcessMode` — an
/// `Error`-severity finding — when they differ, *with one exception*: toggling
/// between `kRealtime` and `kPrefetch` per block is permitted without re-running
/// `setupProcessing`. See `processsetupcheck.cpp` in the VST3 SDK for the rule
/// as written.
///
/// That asymmetry is why this crate splits the two knobs rather than offering
/// one setter: [`Offline`](Self::Offline) is selected at activation
/// ([`Vst3Loaded::activate_with_mode`](crate::Vst3Loaded::activate_with_mode)),
/// because reaching it from any other mode needs a fresh `setupProcessing`,
/// while the realtime/prefetch pair is switchable on a live instance
/// ([`Vst3Active::set_prefetch`](crate::Vst3Active::set_prefetch)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProcessMode {
    /// `kRealtime` — live playback under a deadline. The default.
    #[default]
    Realtime,
    /// `kPrefetch` — realtime playback fed from pre-rendered look-ahead. Freely
    /// interchangeable with [`Realtime`](Self::Realtime) per block; see the type
    /// docs for the spec exception that allows it.
    Prefetch,
    /// `kOffline` — bounce/export with no deadline. Requires its own
    /// `setupProcessing`, so it is fixed for an instance's active lifetime.
    Offline,
}

impl ProcessMode {
    /// The raw `ProcessModes_` value for `ProcessSetup`/`ProcessData`.
    ///
    /// Stays a bare `i32`: both fields are C ABI, which the unit-newtype rule
    /// explicitly carves out.
    pub(crate) const fn to_vst3(self) -> i32 {
        (match self {
            Self::Realtime => ProcessModes_::kRealtime,
            Self::Prefetch => ProcessModes_::kPrefetch,
            Self::Offline => ProcessModes_::kOffline,
        }) as i32
    }

    /// Whether a live instance may switch from `self` to `other` without
    /// re-running `setupProcessing`.
    ///
    /// True only for the `kRealtime`↔`kPrefetch` pair (and the trivial no-op),
    /// mirroring the exception in `ProcessSetupCheck::check`. Any transition
    /// involving [`Offline`](Self::Offline) is false: the checker would report
    /// the resulting setup/data disagreement as an error, and a plugin that
    /// reconfigured for offline work has no reason to expect a realtime block.
    pub(crate) const fn switchable_to(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Realtime, Self::Realtime)
                | (Self::Prefetch, Self::Prefetch)
                | (Self::Realtime, Self::Prefetch)
                | (Self::Prefetch, Self::Realtime)
        )
    }
}

/// Adds the VST3 format-specific FFI details on top of the shared
/// [`tutti_plugin_types::Sample`] trait: the `symbolicSampleSize` tag and the
/// matching `AudioBusBuffers` union member. Generic process code uses
/// `<T: Vst3Sample>` to get both without branching on the format.
pub trait Vst3Sample: tutti_plugin_types::Sample {
    /// The `kSample32` or `kSample64` constant the plugin expects in
    /// `ProcessData::symbolicSampleSize`.
    const VST3_SYMBOLIC_SIZE: i32;

    /// Store a channel-pointer table into `bus`'s buffer union via the member
    /// that matches this format.
    ///
    /// `channelBuffers32`/`channelBuffers64` overlay the same machine pointer (a
    /// pointer's width is independent of its pointee's), so either member writes
    /// the same bytes; the plugin reads whichever [`VST3_SYMBOLIC_SIZE`] names.
    /// Each impl writes only its own member, so the f32 path never mentions an
    /// f64 cast and vice versa.
    ///
    /// [`VST3_SYMBOLIC_SIZE`]: Self::VST3_SYMBOLIC_SIZE
    fn set_channel_buffers(bus: &mut AudioBusBuffers, channel_ptrs: *mut *mut std::ffi::c_void);
}

impl Vst3Sample for f32 {
    const VST3_SYMBOLIC_SIZE: i32 = K_SAMPLE_32_INT;

    fn set_channel_buffers(bus: &mut AudioBusBuffers, channel_ptrs: *mut *mut std::ffi::c_void) {
        bus.__field0.channelBuffers32 = channel_ptrs as *mut *mut f32;
    }
}

impl Vst3Sample for f64 {
    const VST3_SYMBOLIC_SIZE: i32 = K_SAMPLE_64_INT;

    fn set_channel_buffers(bus: &mut AudioBusBuffers, channel_ptrs: *mut *mut std::ffi::c_void) {
        bus.__field0.channelBuffers64 = channel_ptrs as *mut *mut f64;
    }
}

#[cfg(test)]
mod process_mode_tests {
    use super::{ProcessMode, ProcessModes_};

    /// The wire values must be the SDK's, not a re-declared copy. A host that
    /// sends 2 where the plugin reads `kPrefetch` asks for the wrong behaviour
    /// with no error anywhere — the enum ordering is the whole contract.
    #[test]
    fn maps_onto_the_sdk_constants() {
        assert_eq!(
            ProcessMode::Realtime.to_vst3(),
            ProcessModes_::kRealtime as i32
        );
        assert_eq!(
            ProcessMode::Prefetch.to_vst3(),
            ProcessModes_::kPrefetch as i32
        );
        assert_eq!(
            ProcessMode::Offline.to_vst3(),
            ProcessModes_::kOffline as i32
        );
    }

    /// Realtime is the default, so an unannotated activation keeps the
    /// behaviour every existing caller already relies on.
    #[test]
    fn defaults_to_realtime() {
        assert_eq!(ProcessMode::default(), ProcessMode::Realtime);
    }

    /// The live-switch predicate must reproduce `ProcessSetupCheck::check`'s
    /// exception exactly: the realtime/prefetch pair in both directions, and
    /// nothing that touches offline. Enumerated over the full 3x3 rather than
    /// spot-checked, so a later variant cannot quietly widen the exception.
    #[test]
    fn only_realtime_and_prefetch_interchange() {
        use ProcessMode::{Offline, Prefetch, Realtime};
        for (from, to, expected) in [
            (Realtime, Realtime, true),
            (Realtime, Prefetch, true),
            (Realtime, Offline, false),
            (Prefetch, Realtime, true),
            (Prefetch, Prefetch, true),
            (Prefetch, Offline, false),
            (Offline, Realtime, false),
            (Offline, Prefetch, false),
            // Even offline→offline is refused: `set_prefetch` is the only
            // caller and it can never request offline, so allowing it would
            // describe a transition that cannot occur.
            (Offline, Offline, false),
        ] {
            assert_eq!(
                from.switchable_to(to),
                expected,
                "{from:?} -> {to:?} disagrees with the VST3 setup/data agreement rule"
            );
        }
    }
}
