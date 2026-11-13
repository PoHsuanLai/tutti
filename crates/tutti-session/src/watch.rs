//! Watching the host's devices and deciding what to do when they change.
//!
//! [`Follow`] is the decision, a pure step: the time and the roster's view
//! are values. [`DeviceWatch`] is what the session runs on `Phase::Poll`: a
//! [`SnapshotSource`], a [`DeviceRoster`] and a `Follow`. Enumeration is
//! [`DevicePoller`](tutti_cpal::DevicePoller)'s thread, and the diff of two listings is the
//! [`DeviceRoster`]'s; neither runs here.
//!
//! # The policy
//!
//! - Everything that moves the stream is opt-in ([`DevicePolicy`]), and the
//!   session never replaces a device the user chose with another one: a lost
//!   explicit device waits, silent, for its own return or for the host.
//! - The session decides and the host executes. A `Device` is `!Send` and the
//!   host owns it, so a poll reports [`DeviceEvent::RestartWanted`] and the
//!   host calls `LiveSession::restart` with the request, or ignores it to
//!   decline (a strike is counted). Every restart, wanted or the host's own,
//!   is reported by the next poll as [`DeviceEvent::Restarted`] or
//!   [`DeviceEvent::RestartFailed`]. A restart failed by the policy keeps the
//!   old spec and is tried again after the [`Backoff`]'s wait.
//! - A policy restart never makes a choice: the request goes through the same
//!   call as the host's, which drops it as stale when the host has chosen
//!   another device since.
//! - A stream is lost only by a fault ([`DeviceEvent::Fault`]). Absence from a
//!   listing never ends it: ALSA opens every PCM to enumerate, so a device
//!   this process holds can drop out of the list while it plays.
//!   [`DeviceEvent::Lost`] and [`DeviceEvent::Appeared`] mean "no longer
//!   listed" and "listed again".
//! - An unknown device is not a different one. The device a stream runs on is
//!   `None` when the host cannot name it (a driver of the host's own), and
//!   `None` is never compared with the default, so a healthy stream is not
//!   restarted.
//! - On bare ALSA the default's name is constant (`default`), so
//!   [`DeviceEvent::DefaultChanged`] cannot fire there and following the
//!   default moves the stream only after a fault.
//!
//! None of this has run against real hardware, and the cost of enumeration on
//! WASAPI, CoreAudio and JACK is unmeasured, which is why the poll interval is
//! the host's to choose ([`DevicePoller::spawn`](tutti_cpal::DevicePoller::spawn)).

use std::time::{Duration, Instant};

use tutti_cpal::{
    DeviceChange, DeviceId, DeviceListing, DeviceRoster, DeviceSelector, Listed, Snapshot,
    SnapshotSource,
};

use crate::{DeviceEvent, DeviceRestart, SessionEvent};

/// How the retries of one device are spaced.
///
/// Every attempt counts a strike, whatever came of it. The wait before the
/// next is `base * 2^(strikes - 1)`, at most `max`; strikes clear after
/// `settle` of quiet once that wait is over. A device that flaps faster than
/// `settle` is therefore retried every `max`, not on every change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first attempt.
    pub base: Duration,
    /// The longest wait.
    pub max: Duration,
    /// How long without an attempt (after the wait) clears the strikes.
    pub settle: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            settle: Duration::from_secs(10),
        }
    }
}

impl Backoff {
    fn wait(&self, strikes: u32) -> Duration {
        let doublings = strikes.saturating_sub(1).min(20);
        self.base.saturating_mul(1 << doublings).min(self.max)
    }
}

/// What the session may do on its own when devices change. Everything that
/// moves the stream is off by default; the session never replaces an explicit
/// choice with another device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DevicePolicy {
    /// A stream whose selector is `Default` moves when the default output
    /// changes, or when it was lost (a disconnect fault).
    ///
    /// A device missing from the list is **not** a lost stream: ALSA opens
    /// every PCM to enumerate, so a device this process holds can drop out of
    /// the list while it plays. Only a fault ends a stream. On bare ALSA the
    /// default's name is constant (`default`), so the default never
    /// "changes" and only a fault can move the stream. Opening every PCM
    /// every poll is a cost the host opts into, and a restart can meet `EBUSY` from that probe: a counted
    /// strike, then the backoff. A host that is recording should leave this
    /// off, or hold restarts ([`Session::hold_device_restarts`](crate::Session::hold_device_restarts)):
    /// a restart can change the rate mid-take.
    pub follow_default: bool,
    /// The device the host chose explicitly is reopened when it returns.
    pub reconnect: bool,
    /// A start that failed is retried when its device is listed
    /// (a [`Follow::not_started`] policy).
    pub retry_startup: bool,
    /// How retries are spaced.
    pub backoff: Backoff,
}

/// What [`Follow::step`] is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowEvent {
    /// The host selected this device on purpose.
    Chose(DeviceSelector),
    /// The stream is open on this device (`None`: its name is unknown).
    Opened(Option<DeviceId>),
    /// The stream died (a disconnect fault).
    Lost,
    /// A restart failed; `running`: the stream still plays (it was refused
    /// before it stopped).
    RestartFailed {
        /// Whether the old stream still plays.
        running: bool,
    },
    /// Time passed, or the roster changed: look again.
    Look,
}

/// The restart policy as a value: what the user chose, what runs, and when
/// the next attempt may be. [`step`](Self::step) is pure.
#[derive(Clone, Debug)]
pub struct Follow {
    policy: DevicePolicy,
    want: DeviceSelector,
    running_on: Option<DeviceId>,
    started: bool,
    broken: bool,
    held: bool,
    strikes: u32,
    not_before: Option<Instant>,
}

impl Follow {
    /// A stream that is open on `running_on` (`None`: unknown) for the
    /// selector `want`.
    #[must_use]
    pub fn running(
        policy: DevicePolicy,
        want: DeviceSelector,
        running_on: Option<DeviceId>,
    ) -> Self {
        Self {
            policy,
            want,
            running_on,
            started: true,
            broken: false,
            held: false,
            strikes: 0,
            not_before: None,
        }
    }

    /// No stream yet: a start that failed, to retry.
    #[must_use]
    pub fn not_started(policy: DevicePolicy, want: DeviceSelector) -> Self {
        Self {
            started: false,
            broken: true,
            ..Self::running(policy, want, None)
        }
    }

    /// While held, no restart is asked for (events still update the state).
    /// A host that must not have the stream restarted under it (a take being
    /// recorded) holds it.
    pub fn hold(&mut self, held: bool) {
        self.held = held;
    }

    /// The selector the user chose.
    #[must_use]
    pub fn want(&self) -> &DeviceSelector {
        &self.want
    }

    /// Takes `event`, looks at `view` (the devices now) at `now`, and says
    /// which restart to run, if one is due. A restart returned counts a
    /// strike and holds the next for its backoff.
    pub fn step(
        &mut self,
        event: FollowEvent,
        view: &Snapshot,
        now: Instant,
    ) -> Option<DeviceRestart> {
        self.take(event);
        self.decide(view, now)
    }

    /// Takes `event` in without deciding: for an event that arrives before
    /// any look at the devices.
    fn take(&mut self, event: FollowEvent) {
        match event {
            FollowEvent::Chose(sel) => {
                if sel != self.want {
                    self.want = sel;
                    self.strikes = 0;
                    self.not_before = None;
                }
            }
            FollowEvent::Opened(id) => {
                self.running_on = id;
                self.started = true;
                self.broken = false;
            }
            FollowEvent::Lost => self.broken = true,
            FollowEvent::RestartFailed { running } => self.broken |= !running,
            FollowEvent::Look => {}
        }
    }

    fn decide(&mut self, view: &Snapshot, now: Instant) -> Option<DeviceRestart> {
        if self
            .not_before
            .is_some_and(|t| now >= t + self.policy.backoff.settle)
        {
            self.strikes = 0;
            self.not_before = None;
        }
        if self.held {
            return None;
        }
        let device = self.goal(view)?;
        if self.not_before.is_some_and(|t| now < t) {
            return None;
        }
        self.strikes += 1;
        self.not_before = Some(now + self.policy.backoff.wait(self.strikes));
        Some(DeviceRestart::wanted(device))
    }

    fn goal(&self, view: &Snapshot) -> Option<DeviceSelector> {
        let broken = self.broken;
        if !self.started {
            let listed = match &self.want {
                DeviceSelector::Default => view.default_output.is_some(),
                DeviceSelector::Id(id) => view.outputs.contains(id),
            };
            return (self.policy.retry_startup && listed).then(|| self.want.clone());
        }
        match &self.want {
            DeviceSelector::Default => {
                let default = view.default_output.as_ref()?;
                // An unknown device (`None`) is not a different one.
                let moved = self.running_on.as_ref().is_some_and(|d| d != default);
                (self.policy.follow_default && (broken || moved)).then_some(DeviceSelector::Default)
            }
            DeviceSelector::Id(id) => {
                (self.policy.reconnect && broken && view.outputs.contains(id))
                    .then(|| self.want.clone())
            }
        }
    }
}

/// A source, its roster and the policy: what a session runs on `Phase::Poll`
/// ([`LiveSession::watch_devices`](crate::LiveSession::watch_devices)).
pub struct DeviceWatch {
    source: Box<dyn SnapshotSource + Send>,
    roster: DeviceRoster,
    follow: Follow,
    looked: bool,
}

impl std::fmt::Debug for DeviceWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceWatch")
            .field("follow", &self.follow)
            .finish_non_exhaustive()
    }
}

impl DeviceWatch {
    /// Watches for a stream open on `running_on` for the selector `want`.
    pub fn new(
        source: impl SnapshotSource + Send + 'static,
        policy: DevicePolicy,
        want: DeviceSelector,
        running_on: Option<DeviceId>,
    ) -> Self {
        Self {
            source: Box::new(source),
            roster: DeviceRoster::new(),
            follow: Follow::running(policy, want, running_on),
            looked: false,
        }
    }

    /// Tells the policy something happened without deciding, for a decision
    /// that waits for the next [`poll`](Self::poll)'s fresh look.
    pub fn observe(&mut self, event: FollowEvent) {
        self.follow.take(event);
    }

    /// While held, no restart is asked for ([`Follow::hold`]).
    pub fn hold(&mut self, held: bool) {
        self.follow.hold(held);
    }

    /// The devices as last seen.
    #[must_use]
    pub fn roster(&self) -> &DeviceRoster {
        &self.roster
    }

    /// The policy state.
    #[must_use]
    pub fn follow(&self) -> &Follow {
        &self.follow
    }

    /// Tells the policy something happened to the stream.
    pub fn note(&mut self, event: FollowEvent, now: Instant) -> Option<DeviceRestart> {
        if self.looked {
            self.follow.step(event, self.roster.snapshot(), now)
        } else {
            self.follow.take(event);
            None
        }
    }

    /// Takes the newest snapshot, reports what changed, and asks the policy
    /// whether to restart. Before the first snapshot nothing is decided.
    pub fn poll(&mut self, now: Instant, out: &mut Vec<SessionEvent>) {
        if let Some(snapshot) = self.source.poll() {
            self.looked = true;
            for change in self.roster.apply(snapshot) {
                out.push(SessionEvent::Device(match change {
                    DeviceChange::Appeared(direction, id) => {
                        DeviceEvent::Appeared { direction, id }
                    }
                    DeviceChange::Vanished(direction, id) => DeviceEvent::Lost { direction, id },
                    DeviceChange::DefaultChanged(direction, id) => {
                        DeviceEvent::DefaultChanged { direction, id }
                    }
                }));
            }
        }
        if self.looked
            && let Some(request) = self.note(FollowEvent::Look, now)
        {
            out.push(SessionEvent::Device(DeviceEvent::RestartWanted(request)));
        }
    }
}

/// How long a listing of the output devices may wait before the session
/// reports it slow ([`DeviceEvent::OutputsListSlow`]): a healthy host
/// answers in milliseconds, and one that has not in this long is likely stuck.
pub const LISTING_WARNING: Duration = Duration::from_secs(5);

/// A [`DeviceListing`] being waited for: the policy around the thread. The
/// time is a value, as in [`Follow::step`].
#[derive(Debug)]
pub(crate) struct OutputListing {
    listing: DeviceListing,
    /// The `now` of the first poll.
    since: Option<Instant>,
    warned: bool,
}

impl OutputListing {
    pub(crate) fn new(listing: DeviceListing) -> Self {
        Self {
            listing,
            since: None,
            warned: false,
        }
    }

    /// The event this poll has, and whether the listing is over.
    pub(crate) fn poll(&mut self, now: Instant) -> (Option<SessionEvent>, bool) {
        match self.listing.poll() {
            Listed::Devices(devices) => (
                Some(SessionEvent::Device(DeviceEvent::OutputsListed(devices))),
                true,
            ),
            Listed::Failed(why) => (
                Some(SessionEvent::Device(DeviceEvent::OutputsListFailed(why))),
                true,
            ),
            Listed::Pending => {
                let since = *self.since.get_or_insert(now);
                let waited = now.saturating_duration_since(since);
                let slow = waited >= LISTING_WARNING && !self.warned;
                self.warned |= slow;
                (
                    slow.then_some(SessionEvent::Device(DeviceEvent::OutputsListSlow(waited))),
                    false,
                )
            }
        }
    }
}

#[cfg(test)]
mod tests;
