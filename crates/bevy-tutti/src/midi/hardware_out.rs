//! The outbound wire: where drained MIDI goes, and how it is stamped.
//!
//! Two producers feed external hardware — the clock master's ring
//! ([`clock_out`](super::clock_out)) and the track MIDI-out mailbox
//! ([`track_out`](super::track_out)) — and both route through
//! [`MidiOutRouter`], so the stamping decision lives in exactly one place.
//!
//! **JR-out:** with a [`UmpOutRes`] present and [`JrStamperRes`] enabled, each
//! event is JR-stamped — a JR Timestamp prefix derived from its `frame_offset` —
//! and sent as UMP words. With the stamper disabled the events still go out,
//! unstamped; `JrStamperRes` gates timestamps, not delivery.
//!
//! This used to be macOS-only, because `UmpOutRes` wrapped a CoreMIDI
//! `UmpVirtualSource` and the portable path was a lossy MIDI-1.0 port that
//! dropped JR words. Now that every backend yields a `Box<dyn MidiOut>`, there
//! is one path and no `#[cfg]` — a Linux ALSA seq-UMP output stamps too.
//!
//! # Why these live here and not with the metadata they used to
//!
//! [`UmpOutRes`] is a *transport* and [`JrStamperRes`] is its stamping config;
//! neither is Flex Data, and `metadata.rs` — which held both — never used
//! either. They belong beside the router that reads them.

use bevy_ecs::prelude::*;

use tutti_midi_runtime::MidiReceiver;
use tutti_midi_types::ump::MidiEvent;

/// Max events drained per frame into the stack buffer — one frame of clock at
/// any sane tempo is a handful of events, so this is generous headroom.
const DRAIN_CHUNK: usize = 256;

/// Jitter-reduction stamping config.
///
/// Inserted disabled so nothing stamps until an app opts in.
///
/// It gates **stamping, not sending**: with it off, [`UmpOutRes`] still
/// delivers, just without timestamps.
///
/// It holds only the *switch* — the stamper and its running origin belong to the
/// stream being stamped, which is [`UmpOutRes`]'s.
#[derive(Resource, Debug)]
pub struct JrStamperRes {
    /// Whether outbound stamping is active.
    pub enabled: bool,
}

impl JrStamperRes {
    /// Stamping off until an app turns it on.
    pub fn disabled() -> Self {
        Self { enabled: false }
    }

    /// Stamping on.
    pub fn enabled() -> Self {
        Self { enabled: true }
    }
}

impl Default for JrStamperRes {
    fn default() -> Self {
        Self::disabled()
    }
}

/// A UMP MIDI output and its JR stamp stream.
///
/// The sink carries UMP words to the wire, so JR Timestamps and other
/// MIDI-2-only messages survive — which is the whole reason this type exists.
///
/// The [`JrStream`](tutti_midi_runtime::JrStream) is the whole endpoint's, not
/// one pump's: both the clock master and the track path stamp through here, and
/// a per-pump origin would restart each at zero and interleave stamps that walk
/// backwards. bevy-tutti used to carry that origin itself, which is engine state
/// an adapter should not own — it now lives in the engine, where the invariant
/// is structural.
///
/// Not inserted by default; an app that wants JR-out creates the source and
/// inserts this.
#[derive(Resource)]
pub struct UmpOutRes {
    /// The open output. Erased, so this type no longer knows which OS produced
    /// it — which is what let it shed its `#[cfg(target_os = "macos")]`.
    sink: Box<dyn tutti_midi_types::MidiOut>,
    stream: tutti_midi_runtime::JrStream,
    /// Whether to JR-stamp. Mirrored from [`JrStamperRes`] by
    /// [`out_active`] each frame.
    ///
    /// Separate from "is there an output" on purpose: stamping off must still
    /// *send*, just without timestamps. Folding the two together would make
    /// disabling the stamper silently mute MIDI out.
    stamping: bool,
}

impl core::fmt::Debug for UmpOutRes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpOutRes")
            .field("origin_samples", &self.stream.origin_samples())
            .finish_non_exhaustive()
    }
}

impl UmpOutRes {
    /// Wrap a UMP output sink, stamping *and clocking* at `sample_rate`.
    ///
    /// No group: JR Timestamps are groupless utility messages (M2-104-UM
    /// §2.1.2), so a stamp applies to the whole stream.
    ///
    /// This is a UMP-transport sender, so it owns the JR Clock cadence
    /// (§7.2.2.1). Without it the stamps this type exists to emit are inert:
    /// §7.2.2.3 has a receiver that has seen no JR Clock render messages "as
    /// soon as possible", discarding the timing entirely.
    ///
    /// **Previously macOS-only.** It took a `UmpVirtualSource` — a CoreMIDI
    /// type — so JR-stamped output existed on exactly one platform. Taking the
    /// erased sink instead means every backend gets it, which is a capability
    /// gain rather than a refactor: a Linux ALSA seq-UMP output now stamps too.
    pub fn new(sink: Box<dyn tutti_midi_types::MidiOut>, sample_rate: f64) -> Self {
        Self {
            sink,
            stream: tutti_midi_runtime::JrStream::new(sample_rate).with_clock(sample_rate),
            // Off until an app enables `JrStamperRes`, matching that resource's
            // own `disabled()` default.
            stamping: false,
        }
    }

    /// JR-stamp `events` and send each resulting UMP message — the timestamp
    /// prefixes and the events — to the source.
    ///
    /// The stream advances its own origin, so successive blocks stay monotonic
    /// no matter which producer called.
    ///
    /// Prefer [`send_stamped_span`](Self::send_stamped_span): the JR Clock
    /// cadence advances with the *block*, and this entry point can only advance
    /// by the events it was handed, so a silent stream never clocks.
    /// With stamping disabled the events go out unchanged — an unstamped send is
    /// still a send.
    pub fn send_stamped(&mut self, events: &[MidiEvent]) {
        if !self.stamping {
            self.send_all(events);
            return;
        }
        let stamped = self.stream.stamp(events);
        self.send_all(&stamped);
    }

    /// Like [`send_stamped`](Self::send_stamped), but tells the stream how long
    /// the block was so the JR Clock cadence keeps running through silence.
    ///
    /// A pump that knows its block size should call this every block, events or
    /// not — §7.2.2.1 makes JR Clocks independent of other messages, so the
    /// cadence is a property of elapsed time, not of traffic.
    pub fn send_stamped_span(&mut self, events: &[MidiEvent], block_samples: u64) {
        if !self.stamping {
            self.send_all(events);
            return;
        }
        let stamped = self.stream.stamp_span(events, block_samples);
        self.send_all(&stamped);
    }

    fn send_all(&mut self, events: &[MidiEvent]) {
        // One `queue` for the batch, not one per event: `MidiOut` takes a slice,
        // and a backend may pack several UMP messages into one packet.
        self.sink.queue(events);
    }

    /// The absolute sample position the next stamp starts from. For tests and
    /// diagnostics.
    pub fn origin_samples(&self) -> u64 {
        self.stream.origin_samples()
    }
}

/// How many outbound events went nowhere, and whether that has been said aloud.
///
/// `MidiIo::send` pushes into a channel whether or not an output device is
/// connected, and reports the failure at `debug!` — so an app with no output
/// selected drains its mailboxes into silence, forever, with nothing above debug
/// level to show for it. This counts what was lost and warns once, which is the
/// honest halfway house until the host connects something.
#[derive(Resource, Debug, Default)]
pub struct MidiOutDrops {
    dropped: std::sync::atomic::AtomicU64,
    warned: std::sync::atomic::AtomicBool,
}

impl MidiOutDrops {
    /// Events discarded for want of a connected output device.
    pub fn count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Record `n` dropped events, warning the first time only.
    fn record(&self, n: usize) {
        if n == 0 {
            return;
        }
        let total = self
            .dropped
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed)
            + n as u64;
        if !self.warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                "MIDI out has events but no output device is connected; \
                 {total} dropped so far. Send `ConnectMidiOutput` to select one."
            );
        }
    }
}

/// Where drained MIDI-out events go: the open UMP output, else counted as
/// dropped.
///
/// # There is no `#[cfg]` here, and that is the point
///
/// This used to be a ladder — a macOS-only JR-stamped arm that returned early,
/// then a lossy MIDI-1.0 arm, then the drop. Two transports of different
/// capability, chosen by *platform*, so which messages survived depended on
/// where you built.
///
/// It collapses because every backend now yields a `Box<dyn MidiOut>`
/// ([`MidiEndpoints::open_output`](tutti_midi_io::MidiEndpoints::open_output)),
/// so a CoreMIDI port and an ALSA port are one static type. The remaining
/// platform choice happens once, at construction, inside
/// `tutti_midi_io::core::backend::active()`.
pub struct MidiOutRouter<'a> {
    /// The open output, JR-stamping as it sends. `None` when nothing is
    /// connected — which is *why* it is an `Option`: a sink that is present but
    /// silently discarding is what made "nothing is coming out" unanswerable
    /// before.
    pub out: Option<&'a mut UmpOutRes>,
    /// Where events with nowhere to go are counted. `None` keeps the drop
    /// silent, which is what a caller with no interest in the tally passes.
    pub drops: Option<&'a MidiOutDrops>,
}

impl MidiOutRouter<'_> {
    /// Route a batch of drained events to the open output.
    ///
    /// An event with no live transport is dropped — the ring must not back up —
    /// but it is counted first, so "nothing is coming out" has an answer.
    pub fn route(&mut self, events: &[MidiEvent]) {
        match self.out.as_mut() {
            Some(out) => out.send_stamped(events),
            None => {
                if let Some(drops) = self.drops {
                    drops.record(events.len());
                }
            }
        }
    }
}

/// Drain a [`MidiReceiver`] mailbox fully and route every event.
///
/// The single output-drain primitive: the clock master, track MIDI-out, and the
/// clip tap all push into a mailbox via
/// [`MidiOut`](tutti_midi_types::MidiOut), and this reads the paired receiver
/// off-RT. Uses the inherent `poll_into` — the whole mailbox is one output
/// stream, not addressed per-unit.
pub fn drain_receiver_through(receiver: &MidiReceiver, router: &mut MidiOutRouter<'_>) {
    let mut buf = [MidiEvent::noop(); DRAIN_CHUNK];
    loop {
        let n = receiver.poll_into(&mut buf);
        if n == 0 {
            break;
        }
        router.route(&buf[..n]);
        if n < DRAIN_CHUNK {
            break;
        }
    }
}

/// The open output, if there is one.
///
/// [`JrStamperRes`] gates *stamping*, not *sending* — a missing or disabled
/// stamper must still deliver events, unstamped. Conflating the two was the old
/// `jr_out_active`: with the stamper off it returned `None`, which used to fall
/// through to the MIDI-1.0 arm. Now that there is no second arm, returning
/// `None` there would drop every event whenever stamping was off.
pub fn out_active<'a>(
    ump_out: Option<ResMut<'a, UmpOutRes>>,
    jr: Option<Res<'a, JrStamperRes>>,
) -> Option<&'a mut UmpOutRes> {
    let out = ump_out?.into_inner();
    out.stamping = jr.map(|j| j.enabled).unwrap_or(false);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tutti_midi_runtime::tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};

    /// Counts what reached the wire.
    ///
    /// Replaces a real `UmpVirtualSource`, which is why this module is no longer
    /// `#[cfg(target_os = "macos")]`: these are engine invariants, and gating
    /// them on the platform meant CI never ran them at all.
    struct CountingSink(Arc<AtomicUsize>);
    impl tutti_midi_types::MidiOut for CountingSink {
        fn queue(&self, events: &[MidiEvent]) {
            self.0.fetch_add(events.len(), Ordering::SeqCst);
        }
    }

    fn out_with_counter(stamping: bool) -> (UmpOutRes, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let mut out = UmpOutRes::new(Box::new(CountingSink(count.clone())), 48_000.0);
        out.stamping = stamping;
        (out, count)
    }

    fn note(offset: u32) -> MidiEvent {
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
            .with_frame_offset(offset)
    }

    /// Both pumps stamp through one stream, so the origin keeps climbing across
    /// them.
    ///
    /// This is the invariant the wrap-only move bought. When bevy-tutti carried
    /// the origin itself, "the clock master and the track path share it" was a
    /// convention held by a field one of them happened to own; now the engine's
    /// `JrStream` owns it and the two cannot disagree.
    #[test]
    fn both_producers_advance_one_origin() {
        let (mut out, _) = out_with_counter(true);

        out.send_stamped(&[note(511)]);
        assert_eq!(out.origin_samples(), 512, "one 512-frame block");

        out.send_stamped(&[note(511)]);
        assert_eq!(
            out.origin_samples(),
            1024,
            "the second producer continues from the first, it does not restart"
        );
    }

    /// **Stamping off must still send.**
    ///
    /// `JrStamperRes` gates timestamps, not delivery. The old `jr_out_active`
    /// returned `None` when the stamper was disabled, which used to fall through
    /// to the MIDI-1.0 arm — harmless then, and silently muting MIDI out now
    /// that no second arm exists. This pins the distinction.
    #[test]
    fn stamping_disabled_still_delivers_events() {
        let (mut out, count) = out_with_counter(false);
        out.send_stamped(&[note(0), note(1)]);
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "events must reach the sink with stamping off"
        );
        assert_eq!(
            out.origin_samples(),
            0,
            "and the stream must not advance when it did not stamp"
        );
    }

    /// With stamping on, more messages reach the wire than were handed in — the
    /// JR Timestamp prefixes. That is the observable difference between the two
    /// modes, and it is what makes the test above non-vacuous.
    #[test]
    fn stamping_enabled_adds_timestamp_messages() {
        let (mut out, count) = out_with_counter(true);
        out.send_stamped(&[note(0)]);
        assert!(
            count.load(Ordering::SeqCst) > 1,
            "a stamped send carries its JR Timestamp prefix too, got {}",
            count.load(Ordering::SeqCst)
        );
    }

    /// A router with no output counts the events rather than losing them
    /// silently — the whole reason `MidiOutDrops` exists.
    #[test]
    fn routing_with_no_output_counts_the_drops() {
        let drops = MidiOutDrops::default();
        let mut router = MidiOutRouter {
            out: None,
            drops: Some(&drops),
        };
        router.route(&[note(0), note(1), note(2)]);
        assert_eq!(drops.count(), 3);
    }

    /// **`out_active` must return the output whether or not stamping is on.**
    ///
    /// This drives the real function through a `World`, not the `stamping` field
    /// directly — and that distinction is load-bearing. The other tests here set
    /// `stamping` themselves, so they never execute `out_active` and cannot see
    /// it regress. Reverting it to the old `jr_out_active` semantics (stamper
    /// off ⇒ `None`) passed every one of them; only this fails, which is why it
    /// exists.
    ///
    /// With no second transport left to fall through to, returning `None` here
    /// would silently mute MIDI out whenever stamping was disabled — which is
    /// the default.
    #[test]
    fn out_active_yields_the_output_with_stamping_disabled() {
        use bevy_ecs::system::RunSystemOnce;

        fn probe(
            ump_out: Option<ResMut<UmpOutRes>>,
            jr: Option<Res<JrStamperRes>>,
        ) -> (bool, bool) {
            match out_active(ump_out, jr) {
                Some(out) => (true, out.stamping),
                None => (false, false),
            }
        }

        let mut world = World::new();
        world.insert_resource(UmpOutRes::new(
            Box::new(CountingSink(Arc::new(AtomicUsize::new(0)))),
            48_000.0,
        ));
        world.insert_resource(JrStamperRes::disabled());

        let (present, stamping) = world.run_system_once(probe).expect("system runs");
        assert!(
            present,
            "a disabled stamper must NOT hide the output — that would mute MIDI out"
        );
        assert!(!stamping, "and stamping must be off, mirrored from the res");

        world.insert_resource(JrStamperRes::enabled());
        let (present, stamping) = world.run_system_once(probe).expect("system runs");
        assert!(present);
        assert!(stamping, "an enabled stamper turns stamping on");
    }

    /// And a router *with* an output does not count drops.
    #[test]
    fn routing_to_an_open_output_drops_nothing() {
        let (mut out, count) = out_with_counter(false);
        let drops = MidiOutDrops::default();
        let mut router = MidiOutRouter {
            out: Some(&mut out),
            drops: Some(&drops),
        };
        router.route(&[note(0), note(1)]);
        assert_eq!(count.load(Ordering::SeqCst), 2, "both reached the sink");
        assert_eq!(drops.count(), 0, "nothing was dropped");
    }
}
