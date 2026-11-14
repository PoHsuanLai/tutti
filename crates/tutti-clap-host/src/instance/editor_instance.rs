//! The editor-only CLAP instance an out-of-process host loads in its own
//! process ([`OutOfProcess::Editor`](tutti_plugin_types::OutOfProcess)).
//!
//! It is a second load of the same plugin, separate from the audio instance in
//! the plugin server: the host mirrors parameters and state into it and
//! collects the edits the user makes in it.

use std::path::Path;

use tutti_plugin_types::{
    AutomationMode, ContentScale, Control, Editor, EditorCapabilities, EditorError, EditorSize,
    EditorWindow, InProcess, Load, MainThread, Normalized, ParamEdit, ParamId, PluginError,
    PluginResult, Request, RescanScope, TransportRequest, WindowHandle,
};

use super::ClapLoaded;
use super::control::denormalize;
use crate::ParamBounds;
use crate::types::ClapParamFlags;

fn editor_error(reason: String) -> PluginError {
    PluginError::Editor(EditorError::PluginError(reason))
}

/// Look up declared bounds for `id` in a table sorted by id.
fn lookup_range(table: &[ParamBounds], id: u32) -> Option<&ParamBounds> {
    ParamBounds::find(table, id)
}

/// A CLAP plugin loaded for its editor alone: never activated, it shows the
/// GUI and reports the edits made in it.
///
/// It is what [`OutOfProcess::load_editor`](tutti_plugin_types::OutOfProcess)
/// builds: `ClapLoaded` is hostable in this process and over IPC (the twin of
/// the refusal on `Vst2Loaded`, design 021 §3.4):
///
/// ```
/// use tutti_plugin_types::{InProcess, OutOfProcess};
/// fn hostable<F: InProcess + OutOfProcess>() {}
/// hostable::<tutti_clap_host::ClapLoaded>();
/// ```
#[derive(Debug)]
pub struct ClapEditor {
    inner: ClapLoaded,
    /// Declared plain-unit bounds per parameter id, sorted, captured at load.
    ///
    /// CLAP parameter values are in the plugin's own units where this host
    /// speaks normalized, so a mirrored write has to be denormalized. Cached
    /// because `ClapLoaded::parameter_range` is a linear scan that makes one
    /// FFI call per parameter to find one id — acceptable once at load, not on
    /// every knob movement.
    param_ranges: Vec<ParamBounds>,
    /// What the plugin asked of the transport since the last
    /// [`take_transport_requests`](Editor::take_transport_requests).
    transport: Vec<TransportRequest>,
}

impl ClapEditor {
    /// Loads the editor-only instance. It is never activated and never
    /// processes audio: the audio instance runs elsewhere.
    pub(crate) fn load(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        let inner = ClapLoaded::load_in_process(main, path, load)?;
        // One pass over the catalog instead of a scan per write. Params are
        // readable on an unactivated instance, which is what this GUI-only
        // load is.
        let param_ranges = read_param_ranges(main, &inner);

        Ok(Self {
            inner,
            param_ranges,
            transport: Vec::new(),
        })
    }

    /// The loaded plugin behind the editor.
    #[must_use]
    pub fn loaded(&self) -> &ClapLoaded {
        &self.inner
    }

    /// [`loaded`](Self::loaded), mutably.
    pub fn loaded_mut(&mut self) -> &mut ClapLoaded {
        &mut self.inner
    }
}

/// The plugin's declared plain-unit bounds per parameter id, sorted by id.
fn read_param_ranges(main: &MainThread, inner: &ClapLoaded) -> Vec<ParamBounds> {
    let mut ranges: Vec<ParamBounds> = inner
        .native_parameters(main)
        .into_iter()
        .map(|p| ParamBounds {
            id: p.id,
            min: p.min_value,
            max: p.max_value,
            stepped: p.flags.contains(ClapParamFlags::STEPPED),
            read_only: p.flags.contains(ClapParamFlags::READONLY),
        })
        .collect();
    ranges.sort_unstable_by_key(|b| b.id);
    ranges
}

impl Editor for ClapEditor {
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
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
        let bounds = lookup_range(&self.param_ranges, id.get());
        if bounds.is_some_and(|b| b.read_only) {
            return;
        }
        let plain = denormalize(bounds.copied(), value);
        self.inner.set_plain_value(main, id.get(), plain);
    }

    fn set_state(&mut self, main: &MainThread, data: &[u8]) -> PluginResult<()> {
        self.inner
            .load_state(main, data, tutti_plugin_types::StateUse::Project)
            .map_err(|e| editor_error(format!("CLAP set_state failed: {e}")))
    }

    fn poll_edits(&mut self, main: &MainThread) -> Vec<ParamEdit> {
        // A CLAP editor reports its knob movements as output events on the next
        // `params.flush`, so an empty flush is how the host collects them. This
        // is the only bridge back to the audio instance in the subprocess —
        // while it returned nothing, turning a knob in a CLAP editor changed
        // the editor and nothing else, and the edit was lost on close.
        //
        // Calling `flush_params` here is main-thread-legal: CLAP declares
        // `params.flush` as `[active ? audio-thread : main-thread]`, and this
        // instance is loaded for its editor alone and never activated —
        // `ClapLoaded::activate` consumes `self` into a processor and a controller,
        // so a `ClapLoaded` held by value cannot reach the active state. `flush_params` takes the main-thread branch, which is also
        // what the existing `editor_idle` flush above already relies on.
        let events = self.inner.flush_params(main, vec![]);

        // Each event as the edit it reports: a value normalized against the
        // declared bounds (`ClapEvent::heard`), and a gesture's ends.
        events
            .iter()
            .filter_map(|e| e.heard(&self.param_ranges))
            .collect()
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        Control::save_state(&mut self.inner, main, tutti_plugin_types::StateUse::Project)
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        // The editor instance is the one whose UI shows the indication.
        Control::set_automation_state(&mut self.inner, main, mode);
    }

    fn take_transport_requests(&mut self, _main: &MainThread, out: &mut Vec<TransportRequest>) {
        out.append(&mut self.transport);
    }

    fn take_dirty(&mut self, _main: &MainThread) -> bool {
        // `state.mark_dirty` from the editor instance: the user changed
        // something the plugin saves.
        self.inner.take_state_dirty()
    }
}

impl EditorWindow for ClapEditor {
    fn has_editor(&self, main: &MainThread) -> bool {
        EditorWindow::has_editor(&self.inner, main)
    }

    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        let size = self
            .inner
            .embed_editor(main, parent, scale)
            .map_err(|e| editor_error(format!("CLAP open_editor failed: {e}")))?;
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn open_floating_editor(&mut self, main: &MainThread) -> PluginResult<()> {
        // No transient parent: this crate does not hold the host's window, and
        // `set_transient` is a stacking hint the plugin may ignore anyway. The
        // title is what the host would have put on a window it owned.
        let title = std::ffi::CString::new(self.inner.info().name.as_str())
            .unwrap_or_else(|_| c"Plugin Editor".to_owned());
        self.inner
            .open_floating_editor_with(main, None, &title)
            .map_err(|e| editor_error(format!("CLAP floating open failed: {e}")))
    }

    fn close_editor(&mut self, main: &MainThread) {
        EditorWindow::close_editor(&mut self.inner, main);
    }

    fn wakeups(&mut self, main: &MainThread, out: &mut tutti_plugin_types::Wakeups) {
        EditorWindow::wakeups(&mut self.inner, main, out);
    }

    fn wake(
        &mut self,
        main: &MainThread,
        ready: &[tutti_plugin_types::Ready],
        now: std::time::Instant,
    ) {
        EditorWindow::wake(&mut self.inner, main, ready, now);
    }

    fn editor_idle(&mut self, main: &MainThread) {
        // The editor's timers (a UI toolkit's repaint tick), its callbacks to
        // the main thread and its file descriptors are all served from here:
        // this is the only main-thread tick the instance has. What the host's
        // loop found ready (`wake`) is latched, and is taken here.
        let mut requests = Vec::new();
        Control::take_requests(&mut self.inner, main, &mut requests);
        let mut reread = false;
        for request in requests {
            match request {
                Request::Callback => self.inner.run_main_thread(main),
                Request::Timer(id) => self.inner.run_timer(main, id),
                Request::Fd(id) => self.inner.service_fd(main, id),
                // A rescan that re-described the parameters re-reads their bounds.
                Request::RescanParams(RescanScope::Info | RescanScope::All) => reread = true,
                // The plugin's own buttons ask the host of the instance that
                // shows them; the host of the session answers.
                Request::Transport(asked) => self.transport.push(asked),
                // Nothing else is this instance's to serve: it never activates,
                // so a restart or a latency change has nothing to restart.
                _ => {}
            }
        }
        if reread {
            self.param_ranges = read_param_ranges(main, &self.inner);
        }
        // If the plugin requested a param flush, perform it.
        if self.inner.take_params_flush_requested() {
            let _ = self.inner.flush_params(main, vec![]);
        }
    }

    fn editor_capabilities(&mut self, main: &MainThread) -> EditorCapabilities {
        // Pass-through from the host crate — the shared `EditorCapabilities`
        // is already the union of vst3/clap fields, and `appkit_autoresize_friendly`
        // defaults to `false` which matches CLAP's pinned-NSView behavior.
        self.inner.editor_capabilities(main)
    }

    fn set_editor_scale(&mut self, main: &MainThread, scale: ContentScale) -> PluginResult<()> {
        match self.inner.rescale_editor(main, scale) {
            Ok(true) => Ok(()),
            Ok(false) => Err(PluginError::Editor(EditorError::ScaleUnsupported)),
            Err(e) => Err(editor_error(format!("CLAP rescale_editor failed: {e}"))),
        }
    }

    fn set_editor_size(
        &mut self,
        main: &MainThread,
        requested: EditorSize,
    ) -> PluginResult<EditorSize> {
        let snapped = self
            .inner
            .resize_editor(
                main,
                EditorSize {
                    width: requested.width,
                    height: requested.height,
                },
            )
            .map_err(|e| editor_error(format!("CLAP resize_editor failed: {e}")))?;
        Ok(EditorSize {
            width: snapped.width,
            height: snapped.height,
        })
    }

    fn poll_editor_resize_request(&mut self, _main: &MainThread) -> Option<EditorSize> {
        self.inner
            .take_editor_resize_request()
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
    //! Every test here exercises [`ClapEvent::heard`] — the projection — and
    //! none exercises [`ClapEditor::poll_edits`], which needs a real CLAP plugin
    //! behind an FFI boundary to emit output events.
    //!
    //! That gap is stated rather than left to be inferred from the test names,
    //! because it is exactly the kind a reader would assume closed: **reverting
    //! `poll_edits` to its original `Vec::new()` leaves every test
    //! below green.** The contract suite's gesture case covers the wiring in the
    //! audio instance (`ClapProcessor::report_edits`), where the shared plugin
    //! emits one.

    use clap_sys::events::{
        CLAP_EVENT_DONT_RECORD, CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END,
        clap_event_header, clap_event_param_gesture,
    };

    use crate::ClapEvent;

    use super::*;

    /// A cutoff-shaped range: wide, and not `0..=1`, so a conversion is
    /// distinguishable from a pass-through. Both halves matter — against
    /// `0..=1` bounds the plain and normalized domains coincide and every
    /// assertion below would hold with the conversion deleted.
    const CUTOFF: ParamBounds = ParamBounds::continuous(7, 20.0, 20_000.0);

    fn change(id: u32, value: f64) -> Option<ParamEdit> {
        Some(ParamEdit::change(id.into(), Normalized::of(value)))
    }

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
        let ParamBounds { id, min, max, .. } = CUTOFF;

        assert_eq!(
            ClapEvent::param_value(0, id, min).heard(&ranges),
            change(id, 0.0)
        );
        assert_eq!(
            ClapEvent::param_value(0, id, max).heard(&ranges),
            change(id, 1.0)
        );
        // Midpoint of the declared span, which is *not* the midpoint of the
        // value: 10010 Hz is halfway between 20 and 20000 linearly, and a
        // pass-through would report 10010.0 rather than 0.5.
        assert_eq!(
            ClapEvent::param_value(0, id, 10_010.0).heard(&ranges),
            change(id, 0.5)
        );
    }

    /// Round-trip against the sibling `set_parameter` mirror, which converts the
    /// other way against the same table. The two are inverses, so a host write
    /// the editor echoes back must come home unchanged; if either side is
    /// dropped or inverted the value lands somewhere else.
    #[test]
    fn the_normalize_is_the_inverse_of_the_set_parameter_denormalize() {
        let ranges = [CUTOFF];
        let id = CUTOFF.id;

        for host_value in [0.0_f64, 0.25, 0.5, 0.75, 1.0] {
            // What `set_parameter` would hand the plugin...
            let plain = denormalize(Some(CUTOFF), Normalized::of(host_value));
            // ...and what the editor reporting that same position comes back as.
            let heard = ClapEvent::param_value(0, id, plain)
                .heard(&ranges)
                .expect("a PARAM_VALUE event yields an edit");
            let got = heard.value().expect("a change has a value").get();
            assert!(
                (got - host_value).abs() < 1e-9,
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
        assert_eq!(
            ClapEvent::param_value(0, 99, 0.5).heard(&ranges),
            change(99, 0.5)
        );
    }

    /// A note event addresses no parameter, so it is no edit; either one
    /// forwarded would call `set_parameter_rt` with a value that means
    /// something else.
    #[test]
    fn an_event_that_is_not_a_parameter_edit_is_dropped() {
        let ranges = [CUTOFF];
        assert_eq!(ClapEvent::note_on(0, 0, 60, 1.0).heard(&ranges), None);
    }

    fn gesture(type_: u16, param_id: u32) -> clap_event_param_gesture {
        clap_event_param_gesture {
            header: clap_event_header {
                size: size_of::<clap_event_param_gesture>() as u32,
                time: 0,
                space_id: 0,
                type_,
                flags: 0,
            },
            param_id,
        }
    }

    /// `CLAP_EVENT_PARAM_GESTURE_BEGIN` and `_END` are a gesture's ends for
    /// their parameter, with no value of their own.
    #[test]
    fn the_gesture_events_are_a_gestures_ends() {
        let ranges = [CUTOFF];
        assert_eq!(
            ClapEvent::ParamGestureBegin(gesture(CLAP_EVENT_PARAM_GESTURE_BEGIN, 7)).heard(&ranges),
            Some(ParamEdit::Begin(7.into()))
        );
        assert_eq!(
            ClapEvent::ParamGestureEnd(gesture(CLAP_EVENT_PARAM_GESTURE_END, 7)).heard(&ranges),
            Some(ParamEdit::End(7.into()))
        );
    }

    /// A value flagged `CLAP_EVENT_DONT_RECORD` is reported with its value and
    /// the flag, so a host that shows it follows it and one that records
    /// automation does not record it (events.h: "should not be recorded").
    #[test]
    fn a_dont_record_value_is_reported_unrecorded() {
        let ranges = [CUTOFF];
        let mut event = ClapEvent::param_value(0, 7, 10_010.0);
        let ClapEvent::ParamValue(v) = &mut event else {
            unreachable!()
        };
        v.header.flags |= CLAP_EVENT_DONT_RECORD;
        let heard = event.heard(&ranges).expect("a value is an edit");
        assert!(!heard.record());
        assert_eq!(heard.value(), Some(Normalized::of(0.5)));
        assert!(
            ClapEvent::param_value(0, 7, 10_010.0)
                .heard(&ranges)
                .expect("a value is an edit")
                .record()
        );
    }
}
