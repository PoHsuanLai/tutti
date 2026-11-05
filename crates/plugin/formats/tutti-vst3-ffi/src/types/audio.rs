//! VST3-specific sample-format extension on top of `tutti_plugin_types::Sample`.

use vst3::Steinberg::Vst::{AudioBusBuffers, ProcessModes_, SymbolicSampleSizes_};

use crate::sdk::sdk_enum_i32;

/// `SymbolicSampleSizes::kSample32` as the ABI's `int32`.
pub const K_SAMPLE_32_INT: i32 = sdk_enum_i32(SymbolicSampleSizes_::kSample32);
/// `SymbolicSampleSizes::kSample64` as the ABI's `int32`.
pub const K_SAMPLE_64_INT: i32 = sdk_enum_i32(SymbolicSampleSizes_::kSample64);

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
/// ([`Prepare::mode`](tutti_plugin_types::Prepare::mode)), because reaching it
/// from any other mode needs a fresh `setupProcessing`, while the
/// realtime/prefetch pair is switchable on a live instance
/// (the host's prefetch switch).
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
    pub const fn to_vst3(self) -> i32 {
        sdk_enum_i32(match self {
            Self::Realtime => ProcessModes_::kRealtime,
            Self::Prefetch => ProcessModes_::kPrefetch,
            Self::Offline => ProcessModes_::kOffline,
        })
    }

    /// Whether a live instance may switch from `self` to `other` without
    /// re-running `setupProcessing`.
    ///
    /// True only for the `kRealtime`↔`kPrefetch` pair (and the trivial no-op),
    /// mirroring the exception in `ProcessSetupCheck::check`. Any transition
    /// involving [`Offline`](Self::Offline) is false: the checker would report
    /// the resulting setup/data disagreement as an error, and a plugin that
    /// reconfigured for offline work has no reason to expect a realtime block.
    pub const fn switchable_to(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Realtime, Self::Realtime)
                | (Self::Prefetch, Self::Prefetch)
                | (Self::Realtime, Self::Prefetch)
                | (Self::Prefetch, Self::Realtime)
        )
    }
}

/// The sample width a plugin is activated with: which `AudioBusBuffers` union
/// member carries the channel pointers, and which `symbolicSampleSize` the
/// `ProcessData` names.
///
/// This is the whole width-specific surface of the process path; everything
/// else (events, parameter changes, the `ProcessData` build, the call) is the
/// same for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleWidth {
    /// 32-bit floats (`kSample32`).
    F32,
    /// 64-bit floats (`kSample64`).
    F64,
}

impl SampleWidth {
    /// The `symbolicSampleSize` of this width.
    #[must_use]
    pub const fn symbolic_size(self) -> i32 {
        match self {
            Self::F32 => K_SAMPLE_32_INT,
            Self::F64 => K_SAMPLE_64_INT,
        }
    }

    /// Points `bus` at `channel_ptrs` through the union member this width
    /// names. `channelBuffers32`/`channelBuffers64` overlay the same machine
    /// pointer, and the plugin reads whichever [`symbolic_size`](Self::symbolic_size)
    /// names, so each arm writes only its own member.
    pub(crate) fn set_channel_buffers(
        self,
        bus: &mut AudioBusBuffers,
        channel_ptrs: *mut *mut std::ffi::c_void,
    ) {
        match self {
            Self::F32 => bus.__field0.channelBuffers32 = channel_ptrs as *mut *mut f32,
            Self::F64 => bus.__field0.channelBuffers64 = channel_ptrs as *mut *mut f64,
        }
    }
}

#[cfg(test)]
mod process_mode_tests {
    use super::{ProcessMode, ProcessModes_, sdk_enum_i32};

    /// The wire values must be the SDK's, not a re-declared copy. A host that
    /// sends 2 where the plugin reads `kPrefetch` asks for the wrong behaviour
    /// with no error anywhere — the enum ordering is the whole contract.
    #[test]
    fn maps_onto_the_sdk_constants() {
        assert_eq!(
            ProcessMode::Realtime.to_vst3(),
            sdk_enum_i32(ProcessModes_::kRealtime)
        );
        assert_eq!(
            ProcessMode::Prefetch.to_vst3(),
            sdk_enum_i32(ProcessModes_::kPrefetch)
        );
        assert_eq!(
            ProcessMode::Offline.to_vst3(),
            sdk_enum_i32(ProcessModes_::kOffline)
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
