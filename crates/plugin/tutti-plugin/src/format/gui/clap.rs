//! In-process CLAP GUI instance (editor only, no audio processing).
//!
//! Implements the host-side [`PluginEditor`](super::PluginEditor) trait. Though
//! `ClapGuiInstance` wraps the same `ClapLoaded` shape the server-side loader
//! activates, it is a *separate* host-process dlopen from the audio object in
//! the plugin-server subprocess — the two editor surfaces (`PluginEditor` here,
//! `PluginEditorHost` there) stay distinct by design, honestly modelling the
//! two-world split rather than collapsing it.

use super::PluginEditor;
use crate::error::{BridgeError, LoadStage, Result};
use crate::protocol::{Normalized, ParamAddress, ParamRange};
use crate::util::window::{EditorCapabilities, EditorSize, WindowHandle};
use std::path::Path;

/// Look up declared bounds for `id` in a table sorted by id.
fn lookup_range(table: &[(u32, (f64, f64))], id: u32) -> Option<(f64, f64)> {
    table
        .binary_search_by_key(&id, |(k, _)| *k)
        .ok()
        .map(|i| table[i].1)
}

/// One editor-originated output event → the host's `(address, normalized)` pair,
/// or [`None`] for an event that carries no authored value.
///
/// Only `ParamValue` does. `ParamGestureBegin`/`End` mark the start and end of a
/// knob drag and have no value at all, so the trait's `(ParamAddress, f32)` pair
/// has nowhere to put them; surfacing them would mean widening that signature
/// for all three formats, and the only consumer would be automation touch/latch,
/// which is not wired up. `ParamMod` is dropped for a different reason — it is a
/// transient offset *on top of* the value rather than the value, so forwarding
/// it to `set_parameter_rt` would write a modulation excursion into the
/// document.
///
/// The conversion is plain → normalized, the exact inverse of the mirror in
/// [`PluginEditor::set_parameter`]: CLAP speaks the plugin's own units and
/// `set_parameter_rt` speaks `0..=1`. A parameter absent from `ranges` passes
/// through unconverted, matching that mirror — `ParamRange::Normalized` means
/// there is nothing to convert against, and inventing `0..=1` would rescale a
/// parameter the plugin never described.
///
/// Free function rather than a closure so the conversion is reachable without a
/// real plugin behind an FFI boundary.
fn param_edit_from_event(
    ranges: &[(u32, (f64, f64))],
    event: tutti_clap_host::ClapEvent,
) -> Option<(ParamAddress, f32)> {
    let tutti_clap_host::ClapEvent::ParamValue(v) = event else {
        return None;
    };
    let normalized = match lookup_range(ranges, v.param_id) {
        Some((min, max)) => ParamRange::Plain {
            min,
            max,
            default: min,
        }
        .to_normalized(v.value),
        None => v.value,
    };
    Some((ParamAddress::Opaque(v.param_id.into()), normalized as f32))
}

pub(crate) struct ClapGuiInstance {
    inner: tutti_clap_host::ClapLoaded,
    /// Declared plain-unit bounds per parameter id, sorted, captured at load.
    ///
    /// CLAP parameter values are in the plugin's own units where this host
    /// speaks normalized, so a mirrored write has to be denormalized. Cached
    /// because `ClapLoaded::parameter_range` is a linear scan that makes one
    /// FFI call per parameter to find one id — acceptable once at load, not on
    /// every knob movement.
    param_ranges: Vec<(u32, (f64, f64))>,
}

impl ClapGuiInstance {
    pub fn load(path: &Path) -> Result<Self> {
        // Resolve bundle directory to the actual binary.
        let resolved = crate::host::subprocess::resolve_bundle(path)?;
        // Editor-only load: gui/params/state work without activation, and this
        // instance must never be activated or process audio (audio runs in the
        // subprocess instance). `load_editor_only` encodes that contract.
        let inner =
            tutti_clap_host::ClapLoaded::load_editor_only(&resolved, None).map_err(|e| {
                BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Opening,
                    reason: format!("CLAP GUI-only load failed: {e}"),
                }
            })?;
        // One pass over the catalog instead of a scan per write. Params are
        // readable on an unactivated instance, which is what this GUI-only
        // load is.
        let mut param_ranges: Vec<(u32, (f64, f64))> = inner
            .parameter_list()
            .into_iter()
            .filter_map(|p| {
                let id = p.id.opaque()?.get();
                p.bounds().map(|b| (id, b))
            })
            .collect();
        param_ranges.sort_unstable_by_key(|(id, _)| *id);

        Ok(Self {
            inner,
            param_ranges,
        })
    }
}

impl PluginEditor for ClapGuiInstance {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        let clap_handle = unsafe { tutti_clap_host::WindowHandle::from_raw(parent.as_ptr()) };
        let size = self
            .inner
            .open_editor(clap_handle)
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP open_editor failed: {e}")))?;
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn open_floating_editor(&mut self) -> Result<()> {
        // No transient parent: this crate does not hold the host's window, and
        // `set_transient` is a stacking hint the plugin may ignore anyway. The
        // title is what the host would have put on a window it owned.
        let title = std::ffi::CString::new(self.inner.info().name.as_str())
            .unwrap_or_else(|_| c"Plugin Editor".to_owned());
        self.inner
            .open_floating_editor(None, &title)
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP floating open failed: {e}")))
    }

    fn close_editor(&mut self) {
        self.inner.close_editor();
    }

    fn editor_idle(&mut self) {
        // If the plugin requested a param flush, perform it.
        if self.inner.poll_params_flush_requested() {
            let _ = self.inner.flush_params(vec![]);
        }
    }

    fn set_parameter(&mut self, id: ParamAddress, value: Normalized) {
        // `clap_id` is opaque; a VST2 index addresses nothing here.
        let Some(id) = id.opaque() else { return };

        // CLAP values are in the plugin's native plain range, so the host's
        // normalized value is denormalized against the declared bounds — the
        // same conversion the subprocess loader applies on the audio path.
        // Without it, a mirrored write moved the editor to the wrong position
        // while the audio path went where it was asked.
        //
        // A parameter that declared no bounds is written through unconverted,
        // matching the loader: `ParamRange::Normalized` means there is nothing
        // to convert against, and inventing `0..=1` would rescale a parameter
        // the plugin never described.
        let plain = match lookup_range(&self.param_ranges, id.get()) {
            Some((min, max)) => ParamRange::Plain {
                min,
                max,
                default: min,
            }
            .to_plain(value.get()),
            None => value.get(),
        };
        self.inner.set_parameter(id.get(), plain);
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .set_state(data)
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP set_state failed: {e}")))
    }

    fn poll_gui_param_changes(&mut self) -> Vec<(ParamAddress, f32)> {
        // A CLAP editor reports its knob movements as output events on the next
        // `params.flush`, so an empty flush is how the host collects them. This
        // is the only bridge back to the audio instance in the subprocess —
        // while it returned nothing, turning a knob in a CLAP editor changed
        // the editor and nothing else, and the edit was lost on close.
        //
        // Calling `flush_params` here is main-thread-legal: CLAP declares
        // `params.flush` as `[active ? audio-thread : main-thread]`, and this
        // instance is a `load_editor_only` load that is never activated —
        // `ClapLoaded::activate` consumes `self` into a `ClapActive`, and the
        // only other setter of `flags.active` (`reconfigure`) hangs off
        // `ClapActive`, so a `ClapLoaded` held by value cannot reach the active
        // state. `flush_params` takes the main-thread branch, which is also
        // what the existing `editor_idle` flush above already relies on.
        let events = self.inner.flush_params(vec![]);

        events
            .into_iter()
            .filter_map(|e| param_edit_from_event(&self.param_ranges, e))
            .collect()
    }

    fn editor_capabilities(&mut self) -> EditorCapabilities {
        // Pass-through from the host crate — the shared `EditorCapabilities`
        // is already the union of vst3/clap fields, and `appkit_autoresize_friendly`
        // defaults to `false` which matches CLAP's pinned-NSView behavior.
        self.inner.editor_capabilities()
    }

    fn set_editor_size(&mut self, requested: EditorSize) -> Result<EditorSize> {
        let snapped = self
            .inner
            .resize_editor(tutti_clap_host::EditorSize {
                width: requested.width,
                height: requested.height,
            })
            .map_err(|e| BridgeError::ProtocolError(format!("CLAP resize_editor failed: {e}")))?;
        Ok(EditorSize {
            width: snapped.width,
            height: snapped.height,
        })
    }

    fn poll_editor_resize_request(&mut self) -> Option<EditorSize> {
        self.inner
            .poll_editor_resize_request()
            .map(|sz| EditorSize {
                width: sz.width,
                height: sz.height,
            })
    }
}

#[cfg(test)]
mod tests {
    //! What these cover, and what they cannot.
    //!
    //! Every test here exercises [`param_edit_from_event`] — the projection —
    //! and none exercises [`ClapGuiInstance::poll_gui_param_changes`], which
    //! needs a real CLAP plugin behind an FFI boundary to emit output events.
    //!
    //! That gap is stated rather than left to be inferred from the test names,
    //! because it is exactly the kind a reader would assume closed: **reverting
    //! `poll_gui_param_changes` to its original `Vec::new()` leaves every test
    //! below green.** Verified by mutation. The bug this change exists to fix
    //! is pinned at the conversion, not at the wiring, so a refactor that stops
    //! calling the helper would not be caught here. Closing it needs an
    //! integration test against a real plugin, beside `tests/clap_*.rs`.

    use super::*;
    use tutti_clap_host::ClapEvent;

    /// A cutoff-shaped range: wide, and not `0..=1`, so a conversion is
    /// distinguishable from a pass-through. Both halves matter — against
    /// `0..=1` bounds the plain and normalized domains coincide and every
    /// assertion below would hold with the conversion deleted.
    const CUTOFF: (u32, (f64, f64)) = (7, (20.0, 20_000.0));

    /// The editor's plain value is normalized before it leaves for the audio
    /// instance.
    ///
    /// This is the whole point of the bridge: `set_parameter_rt` on the far side
    /// takes `0..=1`, while CLAP reports the plugin's own units. Forwarding
    /// 20000 Hz unconverted would clamp to full scale at the consumer and a
    /// cutoff dragged to 20 Hz would arrive as full-open — the audio going
    /// somewhere the editor is not.
    #[test]
    fn a_plain_editor_value_is_normalized_against_the_declared_range() {
        let ranges = [CUTOFF];
        let (id, (min, max)) = CUTOFF;

        let at_min = param_edit_from_event(&ranges, ClapEvent::param_value(0, id, min));
        assert_eq!(at_min, Some((ParamAddress::Opaque(id.into()), 0.0)));

        let at_max = param_edit_from_event(&ranges, ClapEvent::param_value(0, id, max));
        assert_eq!(at_max, Some((ParamAddress::Opaque(id.into()), 1.0)));

        // Midpoint of the declared span, which is *not* the midpoint of the
        // value: 10010 Hz is halfway between 20 and 20000 linearly, and a
        // pass-through would report 10010.0 rather than 0.5.
        let mid = param_edit_from_event(&ranges, ClapEvent::param_value(0, id, 10_010.0));
        assert_eq!(mid, Some((ParamAddress::Opaque(id.into()), 0.5)));
    }

    /// Round-trip against the sibling `set_parameter` mirror, which converts the
    /// other way against the same table. The two are inverses, so a host write
    /// the editor echoes back must come home unchanged; if either side is
    /// dropped or inverted the value lands somewhere else.
    #[test]
    fn the_normalize_is_the_inverse_of_the_set_parameter_denormalize() {
        let ranges = [CUTOFF];
        let (id, (min, max)) = CUTOFF;
        let range = ParamRange::Plain {
            min,
            max,
            default: min,
        };

        for host_value in [0.0_f64, 0.25, 0.5, 0.75, 1.0] {
            // What `set_parameter` would hand the plugin...
            let plain = range.to_plain(host_value);
            // ...and what the editor reporting that same position comes back as.
            let round_tripped =
                param_edit_from_event(&ranges, ClapEvent::param_value(0, id, plain));
            let (_, got) = round_tripped.expect("a PARAM_VALUE event yields an edit");
            assert!(
                (f64::from(got) - host_value).abs() < 1e-6,
                "{host_value} round-tripped to {got}"
            );
        }
    }

    /// A parameter the plugin declared no bounds for passes through unconverted.
    ///
    /// Same rule the `set_parameter` mirror follows: `ParamRange::Normalized`
    /// means there is nothing to convert against, and inventing `0..=1` would
    /// rescale a parameter the plugin never described. `0.5` is chosen because
    /// it is a legal value in *either* domain, so this asserts the pass-through
    /// rather than a coincidence at an endpoint.
    #[test]
    fn a_parameter_with_no_declared_range_passes_through_unconverted() {
        // Table is non-empty and sorted, but does not contain id 99 — a miss in
        // a populated table, not the trivially-empty case.
        let ranges = [CUTOFF];
        let edit = param_edit_from_event(&ranges, ClapEvent::param_value(0, 99, 0.5));
        assert_eq!(edit, Some((ParamAddress::Opaque(99.into()), 0.5)));
    }

    /// Events that carry no authored value are dropped rather than forwarded.
    ///
    /// The trait pair has no room for a gesture, and a note event addresses no
    /// parameter at all; either one forwarded would call `set_parameter_rt` with
    /// a value that means something else. (`ParamGestureBegin`/`End` are the
    /// motivating case but have no public constructor here — `clap-sys` is not a
    /// dependency of this crate — so the note event stands in for the
    /// non-`ParamValue` arm they share.)
    #[test]
    fn an_event_carrying_no_param_value_is_dropped() {
        let ranges = [CUTOFF];
        assert_eq!(
            param_edit_from_event(&ranges, ClapEvent::note_on(0, 0, 60, 1.0)),
            None
        );
    }
}
