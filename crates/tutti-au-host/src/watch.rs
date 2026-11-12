//! The properties that go stale after load, and the requests a change to one of
//! them raises.
//!
//! Latency and tail are `Float64` **seconds** properties an AU may rewrite at
//! any time: a linear-phase EQ switching modes, an oversampling toggle, a reverb
//! whose decay was turned up. Read once at load, they are a figure PDC keeps
//! compensating and a bounce keeps sizing from long after the plugin stopped
//! agreeing with them. The parameter list is the third: an AU that grows or
//! renames parameters leaves the range table denormalizing automation against
//! bounds that no longer exist.
//!
//! # Why requests rather than values
//!
//! The listener callback arrives on a **GCD queue thread**, not the thread that
//! owns the unit (see [`crate::listener`]). Reading `kAudioUnitProperty_Latency`
//! from there would be a property call on a unit the owning thread may be
//! rendering through, so the callback records only *that* something changed, as
//! a bit of a [`RequestWord`], and the main thread does the read when it takes
//! the word, between blocks (`Control::take_requests`). One bit each: a latency
//! change and a parameter-list change have different consequences downstream,
//! and collapsing them would make every mode switch re-pull the whole parameter
//! list.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use tutti_au_ffi::property::{Latency, ParameterList, TailTime};
use tutti_au_ffi::{AuEvent, AuParameterListener, EventAddress, Property, Unit};
use tutti_plugin_types::{Request, RequestWord, RescanScope};

use crate::edits::ReportedEdits;

/// A live property-change registration on the AU, raising bits of the word it
/// was made with.
///
/// Held by [`AuLoaded`](crate::AuLoaded), declared before the unit so the
/// registration is disposed first: `AuParameterListener` keeps the unit alive
/// itself, but a delivery after the unit's owner has gone is still wasted work.
pub(crate) struct PropertyWatch {
    /// Dropping the last handle runs `AUListenerDispose`, which is the only
    /// thing that stops further deliveries. Shared with the processor, which
    /// names it as the sender of its own parameter writes so they are not heard
    /// back as the plugin's edits.
    listener: Arc<AuParameterListener>,
    /// The parameters whose value changes are subscribed, ascending.
    watched: Vec<u32>,
}

impl PropertyWatch {
    /// Registers on `unit`, watching latency, tail and the parameter list, and
    /// every parameter in `parameters` for the plugin's own edits.
    ///
    /// `None` if AudioToolbox refuses the listener, which leaves the load-time
    /// figures as the whole answer. A refusal is not fatal: an AU whose latency
    /// cannot be watched is still a usable plugin, exactly as one that refuses
    /// to report latency at all is.
    ///
    /// The `watch_property` calls are individually tolerant for a
    /// different reason: `AUEventListenerAddEventType` accepts every property id
    /// without validating it (pinned by
    /// `au_api_surface.rs::registration_never_refuses_a_property_id`), so a
    /// refusal here would be AudioToolbox failing rather than the AU declining a
    /// property, but a failure on one must not cost the other two.
    ///
    /// A stream-format change is not watched: this host sets the stream format
    /// itself at every activation, and the listener cannot tell its own writes
    /// from the unit's, so watching it would report an I/O change after every
    /// activation.
    ///
    /// A parameter's value change is delivered to `edits` in plain units. Only
    /// the value is subscribed: the begin and end of a gesture have no carrier
    /// in the shared vocabulary yet (the `ParamEdit` event of design 029
    /// section 6.1), and a subscription whose events are discarded would only
    /// cost a delivery each.
    pub(crate) fn install(
        unit: &Unit,
        requests: Arc<RequestWord>,
        edits: Arc<ReportedEdits>,
        parameters: impl IntoIterator<Item = u32>,
    ) -> Option<Self> {
        let listener = AuParameterListener::new(unit, move |event| match event {
            AuEvent::PropertyChanged { id, .. } => {
                let request = match id {
                    Latency::ID => Request::LatencyChanged,
                    TailTime::ID => Request::TailChanged,
                    // The list reports which parameters *exist*, so the client
                    // re-pulls it rather than re-reading values against ids that
                    // may be gone.
                    ParameterList::ID => Request::RescanParams(RescanScope::Info),
                    _ => return,
                };
                // `Release`, paired with the `Acquire` swap of the take: the
                // thread that sees the bit is ordered after everything the AU
                // did before posting it.
                request.raise(&requests);
            }
            AuEvent::ParameterChanged { id, address, value } if address == EventAddress::GLOBAL => {
                edits.push(id, value)
            }
            // Gestures are not subscribed, and a parameter off the global
            // scope is not one this host lists.
            _ => {}
        })
        .ok()?;

        for id in [Latency::ID, TailTime::ID, ParameterList::ID] {
            let _ = listener.watch_property(id, EventAddress::GLOBAL);
        }

        let mut watch = Self {
            listener: Arc::new(listener),
            watched: Vec::new(),
        };
        watch.watch_parameters(parameters);
        Some(watch)
    }

    /// Subscribes the value changes of every parameter in `ids` not yet
    /// watched. The registration has no wildcard, so a list that grew needs
    /// this again. One that the AU refuses is left unwatched: its edits are
    /// not heard, which is no worse than before.
    pub(crate) fn watch_parameters(&mut self, ids: impl IntoIterator<Item = u32>) {
        for id in ids {
            if let Err(at) = self.watched.binary_search(&id)
                && self
                    .listener
                    .watch_parameter(id, EventAddress::GLOBAL)
                    .is_ok()
            {
                self.watched.insert(at, id);
            }
        }
    }

    /// The listener, for a writer that names it as the sender of its own writes.
    pub(crate) fn listener(&self) -> &Arc<AuParameterListener> {
        &self.listener
    }
}
