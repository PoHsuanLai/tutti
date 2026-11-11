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

/// A live property-change registration on the AU, raising bits of the word it
/// was made with.
///
/// Held by [`AuLoaded`](crate::AuLoaded), declared before the unit so the
/// registration is disposed first: `AuParameterListener` keeps the unit alive
/// itself, but a delivery after the unit's owner has gone is still wasted work.
pub(crate) struct PropertyWatch {
    /// Never read after construction. Dropping it runs `AUListenerDispose`,
    /// which is the only thing that stops further deliveries.
    _listener: AuParameterListener,
}

impl PropertyWatch {
    /// Registers on `unit`, watching latency, tail and the parameter list.
    ///
    /// `None` if AudioToolbox refuses the listener, which leaves the load-time
    /// figures as the whole answer. A refusal is not fatal: an AU whose latency
    /// cannot be watched is still a usable plugin, exactly as one that refuses
    /// to report latency at all is.
    ///
    /// The three `watch_property` calls are individually tolerant for a
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
    pub(crate) fn install(unit: &Unit, requests: Arc<RequestWord>) -> Option<Self> {
        let listener = AuParameterListener::new(unit, move |event| {
            // Only property changes are subscribed, so a parameter or gesture
            // event here would mean the registration went to the wrong event
            // type; ignore rather than mapping it onto a request it does not
            // mean.
            if let AuEvent::PropertyChanged { id, .. } = event {
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
        })
        .ok()?;

        for id in [Latency::ID, TailTime::ID, ParameterList::ID] {
            let _ = listener.watch_property(id, EventAddress::GLOBAL);
        }

        Some(Self {
            _listener: listener,
        })
    }
}
