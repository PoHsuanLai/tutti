//! Render *mode* — format-agnostic vocabulary for whether a plugin is being
//! processed under realtime pressure.
//!
//! The host tells the plugin whether blocks arrive at a realtime cadence (live
//! playback) or without time pressure (a bounce to disk). A plugin may use the
//! answer to pick a more expensive algorithm — longer FFT windows, more
//! oversampling, a look-ahead it could not afford live — so a render that never
//! sets it silently produces the cheap result in a file the user asked to be
//! exact.
//!
//! Like [`AutomationMode`](crate::AutomationMode), this names the meaningful
//! modes and nothing else; the mapping onto a specific plugin ABI lives at that
//! format's edge in `tutti-plugin`, not here.
//!
//! # Why two variants, when VST3 and VST2 define three
//!
//! Both formats name a third mode between realtime and offline — VST3's
//! `kPrefetch`, VST2's `ProcessLevel::Prefetch` — for blocks that arrive at a
//! *variable* rate but must still be rendered at realtime quality
//! (`ivstaudioprocessor.h:129-132`). It is a live-playback mode, not a render
//! one: an offline bounce explicitly permits *higher* quality and unbounded
//! time, which is the opposite instruction.
//!
//! Neither JUCE nor Ardour ever selects it. JUCE reduces the VST3 enum with
//! `processMode == kOffline`, so `kPrefetch` and `kRealtime` are
//! indistinguishable to a JUCE plugin; every `kPrefetch` token in its tree is
//! vendored SDK header text. Ardour builds `_process_offline ? kOffline :
//! kRealtime` and mentions prefetch exactly once, for an unrelated interface
//! query.
//!
//! So the third variant is omitted because nothing a DAW does wants it, not
//! because CLAP and AU cannot spell it. If variable-rate live playback is ever
//! built, the mode returns with its own justification — and VST3's
//! `IPrefetchableSupport` is already queried, so the plugin's own answer about
//! whether it tolerates prefetch is available to gate it.
//!
//! # Not a per-block field
//!
//! Only VST3 carries this on the block (`ProcessData::processMode`). CLAP's
//! `clap_plugin_render.set` is `[main-thread]` (`ext/render.h:33`), AU's
//! `kAudioUnitProperty_OfflineRender` is a global-scope property a unit may size
//! buffers from at `AudioUnitInitialize`, and VST2 answers it through a host
//! callback. Even VST3 requires `setupProcessing` — a deactivated-state call —
//! to reach `kOffline` (`ivstaudioprocessor.h:142-143`).
//!
//! So it belongs beside sample rate and block size, fixed while the plugin is
//! deactivated, rather than in
//! [`ProcessContext`](crate::ProcessContext).

/// Whether the host is processing this plugin under realtime pressure.
///
/// Set at configure time, not per block — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RenderMode {
    /// Blocks arrive at a realtime cadence; the plugin should process as fast
    /// as it can so other plugins get their slice. The default, because it is
    /// the conservative answer: a plugin told nothing behaves as it does live.
    #[default]
    Realtime,
    /// Blocks arrive without time pressure — an offline bounce or a freewheeling
    /// engine. The plugin may spend more per block for higher quality.
    Offline,
}

impl RenderMode {
    /// Whether this mode is free of realtime pressure.
    ///
    /// Named rather than a bare `== Offline` comparison because every format
    /// edge reduces to this one boolean, and each of them spells it
    /// differently.
    pub fn is_offline(self) -> bool {
        matches!(self, RenderMode::Offline)
    }

    /// The mode implied by whether the caller is rendering offline.
    ///
    /// The inverse of [`is_offline`](Self::is_offline), for the call sites that
    /// hold a bool — an export driver, or an engine reporting that it is
    /// freewheeling.
    pub fn from_offline(offline: bool) -> Self {
        if offline {
            RenderMode::Offline
        } else {
            RenderMode::Realtime
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin told nothing behaves as it does live. Every format defaults
    /// this way (AU's property "defaults to false", CLAP's
    /// `CLAP_RENDER_REALTIME` is 0), and a host that forgets to set it must not
    /// silently opt plugins into a slower path on the audio thread.
    #[test]
    fn the_default_is_realtime() {
        assert_eq!(RenderMode::default(), RenderMode::Realtime);
        assert!(!RenderMode::default().is_offline());
    }

    /// The bool round-trip is what the format edges and the export driver both
    /// use, so it has to be exact in both directions.
    #[test]
    fn offline_round_trips_through_a_bool() {
        for mode in [RenderMode::Realtime, RenderMode::Offline] {
            assert_eq!(RenderMode::from_offline(mode.is_offline()), mode);
        }
    }
}
