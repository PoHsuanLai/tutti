//! One VST3 parameter's [`ParamMapping`]: the plugin's own conversion.

use tutti_plugin_types::{MainThreadMarker, Normalized, ParamMapping};
use tutti_vst3_ffi::Plugin;

/// The controller's `normalizedParamToPlain` and `plainParamToNormalized` for
/// one parameter, whatever shape they have (VST3 fixes none,
/// `ivsteditcontroller.h:508-514`): the plugin's own map, so a decreasing or
/// exponential parameter converts as its editor shows it.
///
/// It borrows the plugin and the [`MainThreadMarker`] token the two controller calls
/// need (`[UI-thread & Connected]`), so it lives for the call that builds it:
/// [`Vst3Loaded::mapping`](crate::Vst3Loaded::mapping).
///
/// The SDK's calls return a bare `ParamValue`, which cannot report a failure, so
/// a controller that answers a value the laws of [`ParamMapping`] exclude is
/// held to them here: a non-finite plain value is `0.0`, and a non-finite plain
/// input reads as position `0.0` without asking the controller.
#[derive(Clone, Copy)]
pub struct ControllerMapping<'a> {
    pub(super) plugin: &'a Plugin,
    pub(super) main: MainThreadMarker,
    pub(super) id: u32,
}

impl ParamMapping for ControllerMapping<'_> {
    fn plain(&self, value: Normalized) -> f64 {
        self.plugin
            .normalized_to_plain(self.main, self.id, value.get())
            .filter(|plain| plain.is_finite())
            .unwrap_or(0.0)
    }

    fn normalized(&self, plain: f64) -> Normalized {
        if !plain.is_finite() {
            return Normalized::of(0.0);
        }
        self.plugin
            .plain_to_normalized(self.main, self.id, plain)
            .map_or(Normalized::of(0.0), Normalized::clamped)
    }
}

impl std::fmt::Debug for ControllerMapping<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerMapping")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
