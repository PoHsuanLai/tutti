//! When a host must tell a plugin the render mode.
//!
//! A plugin may render differently without realtime pressure (a convolution
//! reverb switching to a longer partition, an oversampling saturator raising
//! its factor). Every format carries the request, and
//! [`PluginHandle::set_render_mode`](crate::handles::PluginHandle::set_render_mode)
//! delivers it. Three of the four formats can only take the mode while the
//! plugin is deactivated, so an announcement is a deactivate/reactivate cycle,
//! and a host announces only when it must: [`needs_announcement`].

/// Whether a plugin needs telling the render mode, given whether the mode the
/// host wants moved since its last announcement (`mode_changed`) and whether
/// this plugin has ever been told one (`told`).
///
/// `!told` is not redundant with `mode_changed`. A plugin that finishes
/// loading *during* a bounce arrives when the mode did not move, so a rule
/// keyed only on the change would leave it rendering the rest of the file in
/// whatever mode it defaulted to.
pub const fn needs_announcement(mode_changed: bool, told: bool) -> bool {
    mode_changed || !told
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The complete truth table of `needs_announcement(mode_changed, told)`.
    ///
    /// A two-argument predicate has exactly four inputs, so one table covers
    /// it exhaustively. Each row carries the reason it matters:
    ///
    /// - `(false, true)` is the only `false`, and it is what keeps a host from
    ///   re-announcing every frame: three of the four formats can only take
    ///   the mode while deactivated, so without it every plugin would run a
    ///   deactivate/reactivate cycle per frame.
    /// - `(false, false)` is the mid-render load: a plugin finishing its load
    ///   during a bounce arrives when `mode_changed` is already `false`.
    /// - the two `mode_changed` rows are the ordinary case: a change reaches
    ///   every plugin, told or not.
    ///
    /// Mutations (each run, each failed): `mode_changed` alone (the
    /// `(false, false)` row); `true` (the `(false, true)` row).
    #[test]
    fn needs_announcement_tells_everyone_on_a_change_and_the_untold_always() {
        let cases = [
            (false, true, false, "already told, mode did not move"),
            (
                false,
                false,
                true,
                "a plugin loaded mid-render must still be told the current mode",
            ),
            (true, true, true, "already told, mode moved"),
            (true, false, true, "never told, mode moved"),
        ];

        for (mode_changed, told, expected, why) in cases {
            assert_eq!(
                needs_announcement(mode_changed, told),
                expected,
                "needs_announcement(mode_changed={mode_changed}, told={told}): {why}"
            );
        }
    }
}
