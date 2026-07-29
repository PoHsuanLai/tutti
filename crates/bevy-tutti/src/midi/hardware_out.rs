//! The outbound wire: where drained MIDI goes, and how it is stamped.
//!
//! Two producers feed external hardware — the clock master's ring
//! ([`clock_out`](super::clock_out)) and the track MIDI-out mailbox
//! ([`track_out`](super::track_out)) — and both route through
//! [`MidiOutRouter`], so the JR-stamp / UMP-vs-MIDI-1 decision lives in exactly
//! one place.
//!
//! **JR-out (macOS):** with a [`UmpOutRes`] native-UMP source present and
//! [`JrStamperRes`] enabled, each event is JR-stamped — a JR Timestamp prefix
//! derived from its `frame_offset` — and sent as UMP words. That is the one
//! transport where JR Timestamps reach the wire. Otherwise events go to the
//! MIDI-1.0 port, which drops the JR words, so nothing is stamped for it.
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
/// Inserted disabled so nothing stamps until an app opts in. Only the native-UMP
/// path reads it (see [`UmpOutRes`]); the MIDI-1.0 port cannot carry JR words.
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

/// A native-UMP MIDI output source (macOS) and its JR stamp stream.
///
/// Wraps a [`UmpVirtualSource`](tutti_midi_io::UmpVirtualSource) — a
/// MIDI-2.0-protocol endpoint that carries UMP words to the wire, unlike the
/// MIDI-1.0 [`MidiIoRes`](super::device::MidiIoRes) port that drops JR
/// Timestamps.
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
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
#[derive(Resource, Debug)]
pub struct UmpOutRes {
    source: tutti_midi_io::UmpVirtualSource,
    stream: tutti_midi_runtime::JrStream,
}

#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
impl UmpOutRes {
    /// Wrap a native-UMP source, stamping at `sample_rate`.
    ///
    /// No group: JR Timestamps are groupless utility messages (M2-104-UM
    /// §2.1.2), so a stamp applies to the whole stream.
    pub fn new(source: tutti_midi_io::UmpVirtualSource, sample_rate: f64) -> Self {
        Self {
            source,
            stream: tutti_midi_runtime::JrStream::new(sample_rate),
        }
    }

    /// JR-stamp `events` and send each resulting UMP message — the timestamp
    /// prefixes and the events — to the source.
    ///
    /// The stream advances its own origin, so successive blocks stay monotonic
    /// no matter which producer called.
    pub fn send_stamped(&mut self, events: &[MidiEvent]) {
        let stamped = self.stream.stamp(events);
        for ev in &stamped {
            if let Err(e) = self.source.send_ump(ev.data_words()) {
                tracing::debug!("JR-out UMP send: {e}");
            }
        }
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

/// Where drained MIDI-out events go: JR-stamped UMP if that transport is up and
/// enabled, else the MIDI-1.0 port, else dropped.
pub struct MidiOutRouter<'a> {
    #[cfg(feature = "midi-hardware")]
    pub midi_io: Option<&'a super::device::MidiIoRes>,
    #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
    pub jr_out: Option<&'a mut UmpOutRes>,
    /// Where events with nowhere to go are counted. `None` keeps the drop
    /// silent, which is what a caller with no interest in the tally passes.
    pub drops: Option<&'a MidiOutDrops>,
    /// Keeps the lifetime and the non-hardware build honest (no fields to borrow).
    #[cfg(not(feature = "midi-hardware"))]
    pub _marker: std::marker::PhantomData<&'a ()>,
}

impl MidiOutRouter<'_> {
    /// Route a batch of drained events to the active output transport.
    ///
    /// An event with no live transport is dropped — the ring must not back up —
    /// but it is counted first, so "nothing is coming out" has an answer.
    pub fn route(&mut self, events: &[MidiEvent]) {
        #[cfg(all(target_os = "macos", feature = "midi-hardware"))]
        if let Some(ump) = self.jr_out.as_mut() {
            ump.send_stamped(events);
            return;
        }
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = &self.midi_io {
            // `send` queues regardless of whether a port is open, so the
            // connection check has to happen here: without it every event is
            // accepted and discarded one layer down, at `debug!`.
            if io.0.is_output_connected() {
                for ev in events {
                    io.0.send(*ev);
                }
                return;
            }
        }
        // No hardware feature, no `MidiIo`, or no connected output.
        if let Some(drops) = self.drops {
            drops.record(events.len());
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

/// JR-out is active only with an enabled stamper *and* a native-UMP source.
#[cfg(all(target_os = "macos", feature = "midi-hardware"))]
pub fn jr_out_active<'a>(
    ump_out: Option<ResMut<'a, UmpOutRes>>,
    jr: Option<Res<'a, JrStamperRes>>,
) -> Option<&'a mut UmpOutRes> {
    match (ump_out, jr) {
        (Some(ump), Some(jr)) if jr.enabled => Some(ump.into_inner()),
        _ => None,
    }
}

#[cfg(all(test, target_os = "macos", feature = "midi-hardware"))]
mod tests {
    use super::*;

    /// Both pumps stamp through one stream, so the origin keeps climbing across
    /// them.
    ///
    /// This is the invariant the wrap-only move bought. When bevy-tutti carried
    /// the origin itself, "the clock master and the track path share it" was a
    /// convention held by a field one of them happened to own; now the engine's
    /// `JrStream` owns it and the two cannot disagree.
    #[test]
    fn both_producers_advance_one_origin() {
        let source =
            tutti_midi_io::UmpVirtualSource::new("Test JR-Out").expect("creates ump source");
        let mut out = UmpOutRes::new(source, 48_000.0, 0);

        let clock_block = [MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(511)];
        out.send_stamped(&clock_block);
        assert_eq!(out.origin_samples(), 512, "one 512-frame block");

        let track_block = [MidiEvent::note_on(0, 0, 64, 0x8000).with_frame_offset(511)];
        out.send_stamped(&track_block);
        assert_eq!(
            out.origin_samples(),
            1024,
            "the second producer continues from the first, it does not restart"
        );
    }
}
