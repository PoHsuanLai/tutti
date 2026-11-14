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
/// A plugin may use the answer to pick a more expensive algorithm (longer FFT
/// windows, more oversampling, look-ahead) when rendering offline, so a bounce
/// that never sets it gets the cheap result.
///
/// Set at configure time, beside sample rate and block size, not per block:
/// CLAP's `clap_plugin_render.set` is a main-thread call, AU's
/// `kAudioUnitProperty_OfflineRender` may size buffers at initialization, and
/// VST3 reaches `kOffline` only through `setupProcessing`.
///
/// There is no prefetch variant (VST3 `kPrefetch`, VST2
/// `ProcessLevel::Prefetch`): it is a live-playback mode that no mainstream
/// host selects, and a plugin treats it like realtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
    /// Returns whether this mode is free of realtime pressure.
    ///
    /// Named rather than a bare `== Offline` comparison because every format
    /// edge reduces to this one boolean, and each of them spells it
    /// differently.
    #[must_use]
    pub fn is_offline(self) -> bool {
        matches!(self, RenderMode::Offline)
    }
}

/// How a plugin takes a change of [`RenderMode`], fixed for a loaded plugin.
///
/// Every format has the concept, but they differ in when the plugin reads it,
/// and a plugin may lack it altogether. A host asks this once and acts on the
/// answer: a [`Live`](Self::Live) plugin is told, an
/// [`OnActivation`](Self::OnActivation) one is deactivated and activated again
/// in the new mode, and an [`Unsupported`](Self::Unsupported) one is left
/// running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderModeSupport {
    /// The plugin takes the mode while active: CLAP's `render.set`, or VST2's
    /// answer to `audioMasterGetCurrentProcessLevel`, which the host changes
    /// whenever it likes.
    Live,
    /// The plugin reads the mode only when it is activated: VST3's
    /// `setupProcessing`, AU's `AudioUnitInitialize`. A change is a
    /// deactivation and an activation in the new mode.
    OnActivation,
    /// The plugin has no render mode (a CLAP plugin without `clap.render`, an
    /// Audio Unit that refuses `kAudioUnitProperty_OfflineRender`) and renders
    /// the same either way. A change is recorded for the next activation and
    /// never reactivates the plugin.
    Unsupported,
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
}
