//! A loaded plugin's parameters, mirrored on the control thread: what it
//! declares ([`ParameterInfo`]) and the value of each.
//!
//! The mirror is filled once, by a read that runs off the control thread
//! (each value is a blocking round trip to the plugin), and kept current from
//! the values the plugin reports afterwards (an edit in its GUI, a preset
//! load), through [`PluginHandle::subscribe`], and from the values the host
//! sets ([`report`](PluginParamValues::report)). A value reads as the latest of
//! those at once, before the control thread drains them.
//!
//! **The read is a job, and nothing here spawns it.** [`PluginParamValues::start`]
//! installs the report callback and hands back the mirror plus a `Send`
//! [`ParamRead`]; the host runs [`ParamRead::run`] on whatever thread or pool
//! it owns, and hands the result to [`PluginParamValues::land`]. A host
//! may put it on its task pool, run it on a thread of its
//! own, or inline when it can afford the round trips. A plugin's rescan is the
//! same read again: landing it replaces the declarations and the values
//! together, so a value never outlives the declaration it was read against.
//!
//! **Every format reports** (doc 029 section 6.1): each plugin's own edits reach
//! the parameter callback as [`ParamEdit`](tutti_plugin_types::ParamEdit)s, in
//! process through `Control::take_reported_edits` and through the plugin-server
//! as `BridgeMessage::ParameterChanged`.
//!
//! **An echo is not an edit.** A plugin that reports the value a host just
//! set (from inside `setParameter`) would make that value an edit. The VST2
//! host drops a report made inside its own `setParameter` call
//! (`tutti-vst2-host`'s `host_write`), so none reaches here; a format that
//! reports values in future needs the same filter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::{Notice, PluginHandle, Subscription};
use tutti_plugin_types::{Normalized, ParamId, ParamRange, ParameterInfo};

/// Edits the plugin reported or the host set, in order, waiting for
/// [`PluginParamValues::drain_edits`].
type Edits = Arc<Mutex<Vec<(ParamId, Normalized)>>>;

/// A cloneable way to report values into a mirror from anywhere, as the host
/// sets them ([`PluginParamValues::reporter`]): it outlives a borrow of the
/// mirror and keeps working while the mirror lives (a replaced mirror's queue
/// is never drained).
#[derive(Debug, Clone)]
pub struct ParamReporter(Edits);

impl ParamReporter {
    /// Queues a value as if the plugin had reported it.
    pub fn report(&self, id: ParamId, value: Normalized) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, value));
    }
}

/// What a plugin declares and the value of each parameter, as one read found
/// them: the descriptors, and a value aligned with each.
///
/// Every declared parameter has a value: one the plugin did not answer reads
/// as its declared default.
#[derive(Debug)]
pub struct ParamSnapshot {
    descriptors: Vec<ParameterInfo>,
    values: Vec<Normalized>,
}

/// The blocking half of reading a plugin's parameters: every descriptor and its
/// current value, read through the plugin's handle.
///
/// `Send`, so it runs on any thread. [`run`](Self::run) makes one round trip
/// per parameter, so a host keeps it off a thread that must not block.
#[must_use = "a mirror whose read is dropped never becomes ready; a read that \
              was not answered comes back from `run` to be run again"]
#[derive(Debug)]
pub struct ParamRead {
    handle: PluginHandle,
}

impl ParamRead {
    /// The read of `handle`'s parameters, to run again after the plugin
    /// announced a rescan.
    pub fn of(handle: &PluginHandle) -> Self {
        Self {
            handle: handle.clone(),
        }
    }

    /// Reads every parameter the plugin lists. A parameter whose value the
    /// plugin does not answer reads as its declared default; a parameter listed
    /// twice keeps its first entry; a plugin that lists none gives `Ok` of an
    /// empty snapshot.
    ///
    /// # Errors
    ///
    /// The read itself (boxed: a handle is large), given back to be run again, when the plugin's list is
    /// unavailable (`AnyThread::parameter_descriptors` answered `None`: the
    /// main thread of an in-process plugin did not answer in time, or the
    /// backend cannot list). That is not "lists none": landing an empty snapshot
    /// would make the mirror ready with nothing in it, for good.
    pub fn run(self) -> Result<ParamSnapshot, Box<ParamRead>> {
        let params = self.handle.any_thread();
        let Some(listed) = params.parameter_descriptors() else {
            return Err(Box::new(self));
        };
        let mut seen = std::collections::HashSet::new();
        let mut descriptors = Vec::with_capacity(listed.len());
        let mut values = Vec::with_capacity(listed.len());
        for info in listed {
            if !seen.insert(info.id()) {
                continue;
            }
            let value = params
                .parameter_value(info.id())
                .unwrap_or_else(|| match info.range() {
                    ParamRange::Normalized { default } => Normalized::clamped(default),
                    ParamRange::Plain { default, .. } => params
                        .parameter_normalized(info.id(), default)
                        .unwrap_or_default(),
                });
            descriptors.push(info);
            values.push(value);
        }
        Ok(ParamSnapshot {
            descriptors,
            values,
        })
    }
}

/// What a landed read holds: the declarations and their values, with the
/// position of each id.
#[derive(Debug)]
struct Declared {
    descriptors: Vec<ParameterInfo>,
    values: Vec<Normalized>,
    index: HashMap<ParamId, usize>,
}

/// A loaded plugin's parameters, by id: read once by a [`ParamRead`] (after its
/// saved state is restored), then kept current from the values the plugin
/// reports (`PluginHandle::subscribe`) and the host sets.
///
/// Ids are the `ParamId` a `ParamAddr::Id` names: a CLAP, VST3 or AU parameter
/// id, or a VST2 index.
#[derive(Debug)]
pub struct PluginParamValues {
    declared: Option<Declared>,
    edits: Edits,
    /// The handle's subscription that fills `edits`; the mirror stops
    /// following the plugin when it drops.
    _subscription: Subscription,
}

impl PluginParamValues {
    /// Installs `handle`'s parameter callback, which queues every value the
    /// plugin reports, and returns the mirror (not yet
    /// [ready](Self::is_ready)) with the [`ParamRead`] that fills it. Run the
    /// read off the control thread and pass its result to
    /// [`land`](Self::land).
    ///
    /// The mirror holds its subscription to the handle, so it follows the
    /// plugin until it drops.
    #[must_use = "run the `ParamRead` and `land` its values, or the mirror never becomes ready"]
    pub fn start(handle: &PluginHandle) -> (Self, ParamRead) {
        let edits: Edits = Arc::default();
        let queue = Arc::clone(&edits);
        // The mirror follows the changes; a gesture's ends carry no value.
        let subscription = handle.subscribe(move |notice| {
            if let Notice::Param(edit) = notice
                && let Some(value) = edit.value()
            {
                queue
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((edit.id(), value));
            }
        });
        let mirror = Self {
            declared: None,
            edits,
            _subscription: subscription,
        };
        let read = ParamRead {
            handle: handle.clone(),
        };
        (mirror, read)
    }

    /// Lands a finished [`ParamRead`]: the mirror holds its declarations and
    /// values, replacing any it held, and is [ready](Self::is_ready). Edits
    /// reported meanwhile stay queued for [`drain_edits`](Self::drain_edits),
    /// which applies them over these.
    pub fn land(&mut self, read: ParamSnapshot) {
        let ParamSnapshot {
            descriptors,
            values,
        } = read;
        let index = descriptors
            .iter()
            .enumerate()
            .map(|(at, info)| (info.id(), at))
            .collect();
        self.declared = Some(Declared {
            descriptors,
            values,
            index,
        });
    }

    /// Queues a value as if the plugin had reported it: the host set it, and
    /// reads of it answer it at once.
    pub fn report(&self, id: ParamId, value: Normalized) {
        self.edits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, value));
    }

    /// A handle that reports into this mirror without borrowing it.
    #[must_use]
    pub fn reporter(&self) -> ParamReporter {
        ParamReporter(Arc::clone(&self.edits))
    }

    /// Every parameter the plugin declares, in the order it listed them; empty
    /// until the read has landed.
    #[must_use]
    pub fn descriptors(&self) -> &[ParameterInfo] {
        self.declared.as_ref().map_or(&[], |d| &d.descriptors)
    }

    /// Where parameter `id` is in [`descriptors`](Self::descriptors); `None`
    /// for one the plugin does not declare (or before the read lands).
    #[must_use]
    pub fn position(&self, id: ParamId) -> Option<usize> {
        self.declared.as_ref()?.index.get(&id).copied()
    }

    /// The value of the parameter at `position` in
    /// [`descriptors`](Self::descriptors): the latest the plugin reported or the
    /// host set, else the last one drained.
    ///
    /// # Panics
    ///
    /// If `position` is not one [`position`](Self::position) returned for the
    /// current read: a landed read replaces the positions, so a position is
    /// never kept across one.
    #[must_use]
    pub fn value_at(&self, position: usize) -> Normalized {
        let declared = self
            .declared
            .as_ref()
            .expect("a position comes from a landed read");
        let id = declared.descriptors[position].id();
        self.latest(id).unwrap_or_else(|| declared.values[position])
    }

    /// The newest value queued for `id`, if one is.
    fn latest(&self, id: ParamId) -> Option<Normalized> {
        self.edits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .rev()
            .find_map(|&(at, value)| (at == id).then_some(value))
    }

    /// Every value held, by id; empty until the read has landed.
    pub fn values(&self) -> impl Iterator<Item = (ParamId, Normalized)> + '_ {
        self.declared.iter().flat_map(|d| {
            d.descriptors
                .iter()
                .zip(&d.values)
                .map(|(info, &value)| (info.id(), value))
        })
    }

    /// Whether the load-time read has landed.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.declared.is_some()
    }

    /// Parameter `id`'s value: `None` while the read is in flight, `Some(None)`
    /// for a parameter the plugin did not list.
    pub fn get(&self, id: ParamId) -> Option<Option<Normalized>> {
        let declared = self.declared.as_ref()?;
        Some(
            declared
                .index
                .get(&id)
                .map(|&at| self.latest(id).unwrap_or(declared.values[at])),
        )
    }

    /// Applies the values the plugin reported or the host set since the last
    /// call, in order: each goes into the mirror and then to `apply` (a host
    /// moves the knob of the parameter's modulation lane with it). A value for a
    /// parameter the plugin does not declare reaches `apply` and is not kept.
    ///
    /// Does nothing until the read has landed, and the edits wait: one
    /// applied before it would be overwritten by a value read earlier. The
    /// other order has a window of its own: an edit reported while the read
    /// is running, before the read reaches its parameter, is applied after it
    /// and so replaces the value read, though the read is newer. Only a value
    /// changed again within that window without a report is affected (each
    /// change the plugin reports arrives here in order), and the next
    /// report corrects it.
    pub fn drain_edits(&mut self, mut apply: impl FnMut(ParamId, Normalized)) {
        let Some(declared) = self.declared.as_mut() else {
            return;
        };
        let edits = std::mem::take(&mut *self.edits.lock().unwrap_or_else(PoisonError::into_inner));
        for (id, value) in edits {
            if let Some(&at) = declared.index.get(&id) {
                declared.values[at] = value;
            }
            apply(id, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn n(v: f64) -> Normalized {
        Normalized::clamped(v)
    }

    use crate::ParameterChangeSink;
    use tutti_plugin_types::ParameterInfo;
    use tutti_plugin_types::StateError;
    use tutti_plugin_types::{
        AnyThread, AutomationMode, AutomationModeError, Health, PluginFailure, RenderMode,
        TerminationReason,
    };
    use tutti_plugin_types::{Normalized, ParamId};

    /// A backend listing `params` (address, value); a value of `None` is a
    /// parameter it lists but does not answer.
    struct FakeParams {
        params: Vec<(ParamId, Option<f32>)>,
    }

    impl AnyThread for FakeParams {
        fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
            Some(
                self.params
                    .iter()
                    .map(|&(id, _)| ParameterInfo::new(id, "p"))
                    .collect(),
            )
        }
        fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
            self.params
                .iter()
                .find(|(a, _)| *a == id)?
                .1
                .map(|v| Normalized::clamped(f64::from(v)))
        }
        fn set_parameter_value(&self, _id: ParamId, _value: Normalized) {}
        fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
            None
        }
        fn parameter_normalized(&self, _id: ParamId, _plain: f64) -> Option<Normalized> {
            None
        }
        fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
            None
        }
        fn save_state(&self) -> Result<Vec<u8>, StateError> {
            Err(StateError::NoStateRoute)
        }
        fn set_render_mode(&self, _mode: RenderMode) -> bool {
            false
        }
        fn set_automation_mode(&self, _mode: AutomationMode) -> Result<(), AutomationModeError> {
            Err(AutomationModeError::PluginDead)
        }
    }

    impl Health for FakeParams {
        fn is_crashed(&self) -> bool {
            false
        }
        fn crash_cause(&self) -> Option<PluginFailure> {
            None
        }
        fn rate_refusal(&self) -> Option<(tutti_types::SampleRate, String)> {
            None
        }
        fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
            tutti_plugin_types::BlockCounts::default()
        }
        fn terminate(&self, _reason: TerminationReason) {}
    }

    /// A handle over [`FakeParams`], and the sink its reports go through
    /// (firing it is the plugin reporting a value).
    fn handle(params: Vec<(ParamId, Option<f32>)>) -> (PluginHandle, ParameterChangeSink) {
        let sink = ParameterChangeSink::default();
        let handle = PluginHandle::from_backend(
            Arc::new(FakeParams { params }),
            None,
            Default::default(),
            Default::default(),
            sink.clone(),
        );
        (handle, sink)
    }

    /// `ParamRead::run` returns each listed parameter's descriptor and value by
    /// its id, and `land` makes the mirror ready with them.
    ///
    /// The fake lists two parameters and one it does not answer: the first two
    /// come back with their values, and the third reads as its declared default
    /// (`0.0`, the normalized default the fake's descriptor declares), so every
    /// declared parameter has a value.
    #[test]
    fn a_read_returns_the_descriptors_values_and_landing_it_makes_the_mirror_ready() {
        use tutti_plugin_types::ParamId;
        let opaque = ParamId::new(7);
        let index = ParamId::new(3);
        let silent = ParamId::new(9);
        let unlisted = ParamId::new(11);
        let (plugin, _) = handle(vec![
            (opaque, Some(0.25)),
            (index, Some(0.75)),
            (silent, None),
        ]);

        let (mut mirror, read) = PluginParamValues::start(&plugin);
        assert!(!mirror.is_ready(), "the mirror waits for its read");
        assert_eq!(
            mirror.get(opaque),
            None,
            "nothing is known before the read lands"
        );
        assert!(mirror.descriptors().is_empty());

        // On another thread, as a host runs it: the job is `Send`.
        let snapshot = std::thread::spawn(move || read.run())
            .join()
            .unwrap()
            .expect("the plugin lists its parameters");

        mirror.land(snapshot);
        assert!(mirror.is_ready());
        let listed: Vec<ParamId> = mirror.descriptors().iter().map(|p| p.id()).collect();
        assert_eq!(listed, [opaque, index, silent], "in the plugin's order");
        assert_eq!(mirror.get(opaque), Some(Some(n(0.25))));
        assert_eq!(mirror.get(index), Some(Some(n(0.75))));
        assert_eq!(
            mirror.get(silent),
            Some(Some(n(0.0))),
            "an unanswered parameter reads as its declared default"
        );
        assert_eq!(mirror.get(unlisted), Some(None), "not declared: not listed");
        assert_eq!(mirror.position(index), Some(1));
        assert_eq!(mirror.position(unlisted), None);
    }

    /// A value the plugin reports reaches the mirror through the callback
    /// `start` installs, and waits for the read: drained before it lands it
    /// is held, not lost and not applied.
    #[test]
    fn a_reported_edit_waits_for_the_read_then_reaches_the_mirror() {
        use tutti_plugin_types::{ParamEdit, ParamId};
        let (plugin, sink) = handle(vec![(ParamId::new(1), Some(0.1))]);
        let (mut mirror, read) = PluginParamValues::start(&plugin);

        sink.fire(ParamEdit::change(ParamId::new(1), n(0.9)));
        let mut seen = Vec::new();
        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert!(seen.is_empty(), "an edit before the read lands waits");

        mirror.land(read.run().expect("the plugin lists its parameters"));
        assert_eq!(
            mirror.get(ParamId::new(1)),
            Some(Some(n(0.9))),
            "a queued report reads at once, before it is drained"
        );
        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert_eq!(seen, [(ParamId::new(1), n(0.9))]);
        assert_eq!(
            mirror.get(ParamId::new(1)),
            Some(Some(n(0.9))),
            "the edit replaces the value read"
        );

        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert_eq!(
            seen,
            [(ParamId::new(1), n(0.9))],
            "a drained edit is consumed, not applied again"
        );
    }

    /// A value the host sets reads back at once and, once drained, stays; a
    /// second read landing replaces the declarations and the values together.
    #[test]
    fn a_host_set_reads_back_and_a_landed_read_replaces_everything() {
        use tutti_plugin_types::ParamId;
        let id = ParamId::new(5);
        let (plugin, _) = handle(vec![(id, Some(0.2))]);
        let (mut mirror, read) = PluginParamValues::start(&plugin);
        mirror.land(read.run().expect("lists"));

        mirror.report(id, n(0.6));
        assert_eq!(mirror.get(id), Some(Some(n(0.6))), "read-your-writes");
        let at = mirror.position(id).expect("declared");
        assert_eq!(mirror.value_at(at), n(0.6));
        mirror.drain_edits(|_, _| {});
        assert_eq!(mirror.get(id), Some(Some(n(0.6))), "kept after the drain");

        let (other, _) = handle(vec![(ParamId::new(8), Some(0.5))]);
        let (_, read) = PluginParamValues::start(&other);
        mirror.land(read.run().expect("lists"));
        assert_eq!(mirror.get(id), Some(None), "the old declaration is gone");
        assert_eq!(mirror.get(ParamId::new(8)), Some(Some(n(0.5))));
    }
}
