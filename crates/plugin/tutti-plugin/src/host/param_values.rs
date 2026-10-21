//! A loaded plugin's parameter values, mirrored on the control thread.
//!
//! The mirror is filled once, by a read that runs off the control thread
//! (each value is a blocking round trip to the plugin), and kept current from
//! the values the plugin reports afterwards (an edit in its GUI, a preset
//! load), through [`PluginHandle::on_parameter_changed`].
//!
//! **The read is a job, and nothing here spawns it.** [`PluginParamValues::start`]
//! installs the report callback and hands back the mirror plus a `Send`
//! [`ParamRead`]; the host runs [`ParamRead::run`] on whatever thread or pool
//! it owns, and hands the result to [`PluginParamValues::land`]. A Bevy host
//! puts it on its task pool; a headless one may run it on a thread of its
//! own, or inline when it can afford the round trips.
//!
//! **Reports are VST2's only, today.** Of the formats, only VST2's
//! `audioMasterAutomate` reaches the parameter callback (in process, and
//! through the plugin-server's `ParameterChanged`). CLAP, VST3 and AU emit no
//! per-value report through the server (`tutti-plugin-server`'s
//! `Plugin::poll_async_events`).
//!
//! **An echo is not an edit.** A plugin that reports the value a host just
//! set (from inside `setParameter`) would make that value an edit. The VST2
//! host drops a report made inside its own `setParameter` call
//! (`tutti-vst2-host`'s `host_write`), so none reaches here; a format that
//! reports values in future needs the same filter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::host::handles::{ParamAddress, PluginHandle};

/// Edits the plugin reported, in order, waiting for [`PluginParamValues::drain_edits`].
type Edits = Arc<Mutex<Vec<(u32, f32)>>>;

/// A loaded plugin's parameter values, by id: read once by a [`ParamRead`]
/// (after its saved state is restored), then kept current from the values
/// the plugin reports (`PluginHandle::on_parameter_changed`).
///
/// Ids are the `u32` a `ParamAddr::Id` names: a CLAP, VST3 or AU parameter
/// id, or a VST2 index. Values are the plugin's own `f32`, untyped because
/// they cross the plugin boundary as the format gives them.
pub struct PluginParamValues {
    values: Option<HashMap<u32, f32>>,
    edits: Edits,
}

/// The blocking half of reading a plugin's parameter values: every
/// descriptor's current value, read through the plugin's handle.
///
/// `Send`, so it runs on any thread. [`run`](Self::run) makes one round trip
/// per parameter, so a host keeps it off a thread that must not block.
#[must_use = "a mirror whose read is dropped never becomes ready"]
pub struct ParamRead {
    handle: PluginHandle,
}

impl ParamRead {
    /// Reads every parameter the plugin lists, by id. A parameter whose
    /// address has no `u32` id, or whose value the plugin does not answer, is
    /// left out; a plugin that lists none gives an empty map.
    pub fn run(self) -> HashMap<u32, f32> {
        let params = self.handle.params();
        params
            .parameter_descriptors()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                let id = param_id(p.id)?;
                Some((id, params.parameter_value(p.id)?))
            })
            .collect()
    }
}

impl PluginParamValues {
    /// Installs `handle`'s parameter callback, which queues every value the
    /// plugin reports, and returns the mirror (not yet
    /// [ready](Self::is_ready)) with the [`ParamRead`] that fills it. Run the
    /// read off the control thread and pass its result to
    /// [`land`](Self::land).
    ///
    /// The callback takes the handle's single `on_parameter_changed` slot,
    /// replacing any callback installed before, and one installed after
    /// replaces it (the mirror then stops following the plugin).
    #[must_use = "run the `ParamRead` and `land` its values, or the mirror never becomes ready"]
    pub fn start(handle: &PluginHandle) -> (Self, ParamRead) {
        let edits: Edits = Arc::default();
        let queue = Arc::clone(&edits);
        handle.on_parameter_changed(move |id, value| {
            queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((id, value));
        });
        let mirror = Self {
            values: None,
            edits,
        };
        let read = ParamRead {
            handle: handle.clone(),
        };
        (mirror, read)
    }

    /// Values known from the start, with no plugin behind them (a test's, or
    /// a host that mirrors values its own way). Edits arrive through
    /// [`report`](Self::report).
    pub fn known(values: impl IntoIterator<Item = (u32, f32)>) -> Self {
        Self {
            values: Some(values.into_iter().collect()),
            edits: Arc::default(),
        }
    }

    /// Lands a finished [`ParamRead`]: the mirror holds `values` and is
    /// [ready](Self::is_ready). Edits reported meanwhile stay queued for
    /// [`drain_edits`](Self::drain_edits), which applies them over these.
    pub fn land(&mut self, values: HashMap<u32, f32>) {
        self.values = Some(values);
    }

    /// Queues a value as if the plugin had reported it.
    pub fn report(&self, id: u32, value: f32) {
        self.edits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, value));
    }

    /// Whether the load-time read has landed.
    pub fn is_ready(&self) -> bool {
        self.values.is_some()
    }

    /// Parameter `id`'s value: `None` while the read is in flight, `Some(None)`
    /// for a parameter the plugin did not list.
    pub fn get(&self, id: u32) -> Option<Option<f32>> {
        self.values.as_ref().map(|v| v.get(&id).copied())
    }

    /// Applies the values the plugin reported since the last call, in order:
    /// each goes into the mirror and then to `apply` (a host moves the knob
    /// of the parameter's modulation lane with it).
    ///
    /// Does nothing until the read has landed, and the edits wait: one
    /// applied before it would be overwritten by a value read earlier. The
    /// other order has a window of its own: an edit reported while the read
    /// is running, before the read reaches its parameter, is applied after it
    /// and so replaces the value read, though the read is newer. Only a value
    /// changed again within that window without a report is affected (each
    /// change the plugin reports arrives here in order), and the next report
    /// corrects it.
    pub fn drain_edits(&mut self, mut apply: impl FnMut(u32, f32)) {
        let Some(values) = self.values.as_mut() else {
            return;
        };
        let edits = std::mem::take(&mut *self.edits.lock().unwrap_or_else(PoisonError::into_inner));
        for (id, value) in edits {
            values.insert(id, value);
            apply(id, value);
        }
    }
}

/// The `u32` a [`ParamAddress`] names in `ParamAddr::Id`.
fn param_id(id: ParamAddress) -> Option<u32> {
    match id {
        ParamAddress::Opaque(id) => Some(id.get()),
        ParamAddress::Index(i) => u32::try_from(i).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{HostParams, HostState, ParameterChangeSink};
    use crate::error::StateError;
    use crate::handles::{Normalized, OptionalCapabilities};
    use crate::server::ParameterInfo;

    /// A backend listing `params` (address, value); a value of `None` is a
    /// parameter it lists but does not answer.
    struct FakeParams {
        params: Vec<(ParamAddress, Option<f32>)>,
    }

    impl HostParams for FakeParams {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
            Some(
                self.params
                    .iter()
                    .map(|&(id, _)| ParameterInfo::new(id, "p"))
                    .collect(),
            )
        }
        fn parameter_value(&self, id: ParamAddress) -> Option<f32> {
            self.params.iter().find(|(a, _)| *a == id)?.1
        }
        fn set_parameter_value(&self, _id: ParamAddress, _value: Normalized) {}
        fn is_crashed(&self) -> bool {
            false
        }
        fn crash_cause(&self) -> Option<String> {
            None
        }
        fn dropped_parameter_events(&self) -> u64 {
            0
        }
    }

    impl HostState for FakeParams {
        fn save_state(&self) -> Result<Vec<u8>, StateError> {
            Err(StateError::NoStateRoute)
        }
        fn load_state(&self, _data: &[u8]) -> Result<(), StateError> {
            Ok(())
        }
    }

    /// A handle over [`FakeParams`], and the sink its reports go through
    /// (firing it is the plugin reporting a value).
    fn handle(params: Vec<(ParamAddress, Option<f32>)>) -> (PluginHandle, ParameterChangeSink) {
        let sink = ParameterChangeSink::default();
        let handle = PluginHandle::from_backend(
            Arc::new(FakeParams { params }),
            OptionalCapabilities::default(),
            Default::default(),
            Default::default(),
            sink.clone(),
        );
        (handle, sink)
    }

    /// `ParamRead::run` returns each listed parameter's value by its id, and
    /// `land` makes the mirror ready with them.
    ///
    /// The fake lists an opaque id, a VST2 index and a parameter it does not
    /// answer: the first two come back by their `u32`, the third is left out
    /// (`Some(None)` once landed: listed nowhere the mirror knows).
    ///
    /// Mutations (each run, each failed): `run` returning an empty map (the
    /// values assert fails); `land` dropping its values (`is_ready` fails);
    /// `param_id` mapping `Index(i)` to `None` (the index's value is missing).
    #[test]
    fn a_read_returns_the_descriptors_values_and_landing_it_makes_the_mirror_ready() {
        use crate::handles::ParamId;
        let opaque = ParamAddress::Opaque(ParamId::new(7));
        let index = ParamAddress::Index(3);
        let silent = ParamAddress::Opaque(ParamId::new(9));
        let (plugin, _) = handle(vec![
            (opaque, Some(0.25)),
            (index, Some(0.75)),
            (silent, None),
        ]);

        let (mut mirror, read) = PluginParamValues::start(&plugin);
        assert!(!mirror.is_ready(), "the mirror waits for its read");
        assert_eq!(
            mirror.get(7),
            None,
            "nothing is known before the read lands"
        );

        // On another thread, as a host runs it: the job is `Send`.
        let values = std::thread::spawn(move || read.run()).join().unwrap();
        assert_eq!(values, HashMap::from([(7, 0.25), (3, 0.75)]));

        mirror.land(values);
        assert!(mirror.is_ready());
        assert_eq!(mirror.get(7), Some(Some(0.25)));
        assert_eq!(mirror.get(3), Some(Some(0.75)));
        assert_eq!(
            mirror.get(9),
            Some(None),
            "an unanswered parameter is not listed"
        );
    }

    /// A value the plugin reports reaches the mirror through the callback
    /// `start` installs, and waits for the read: drained before it lands it
    /// is held, not lost and not applied.
    ///
    /// Mutations (each run, each failed): `start` not installing the callback
    /// (nothing is drained); `drain_edits` applying before the read lands (the
    /// first drain sees the edit); `drain_edits` clearing the queue before the
    /// read lands (the second drain sees nothing); `drain_edits` cloning the
    /// queue instead of taking it (the third drain applies the edit again).
    #[test]
    fn a_reported_edit_waits_for_the_read_then_reaches_the_mirror() {
        use crate::handles::ParamId;
        let (plugin, sink) = handle(vec![(ParamAddress::Opaque(ParamId::new(1)), Some(0.1))]);
        let (mut mirror, read) = PluginParamValues::start(&plugin);

        sink.fire(1, 0.9);
        let mut seen = Vec::new();
        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert!(seen.is_empty(), "an edit before the read lands waits");

        mirror.land(read.run());
        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert_eq!(seen, [(1, 0.9)]);
        assert_eq!(
            mirror.get(1),
            Some(Some(0.9)),
            "the edit replaces the value read"
        );

        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert_eq!(
            seen,
            [(1, 0.9)],
            "a drained edit is consumed, not applied again"
        );
    }
}
