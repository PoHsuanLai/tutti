//! [`MidiPreBlock`] — the once-per-block MIDI producer that runs *before* the
//! graph renders.
//!
//! Every event-consuming node owns a [`MidiInPort`](crate::MidiInPort) inbox,
//! drains the whole block in one poll, and times each event by its
//! `frame_offset` (see `PolySynth`, `SoundFontUnit`, and the plugin nodes). So
//! the audio callback needs only to have this block's events *delivered into
//! those inboxes* before it renders — which is exactly this producer's job.
//! Once per audio block, before the graph's `process`, it:
//!
//! 1. ticks the outbound clock/timecode generator ([`BlockClock`]);
//! 2. polls the hardware MIDI input for the whole block;
//! 3. applies input-edge translation to each event — (N)RPN assembly
//!    ([`Midi1ToMidi2Translator`]) then classic-MPE → native-per-note rewriting
//!    ([`MpeIngest`]) — so downstream nodes see only native MIDI-2 per-note
//!    messages (per M2-104: MPE zone/channel-spread is an *ingestion* concern);
//! 4. routes each event to its destination unit's inbox via the [`MidiRouter`]
//!    fan-out — each event keeps its own `frame_offset`, so the consuming node
//!    times it correctly.
//!
//! Every method is **lock-free** and **alloc-free**, safe on the audio thread.

use std::sync::Arc;

use arc_swap::ArcSwap;

use tutti_core::{AudioThreadCell, RtEventBuf};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiIn, MidiRouter, MidiRoutingSnapshot, MidiUnitId};
use tutti_midi_types::Midi1ToMidi2Translator;

use crate::mpe_ingest::MpeIngest;

/// Per-block outbound clock/timecode generator (e.g. a `ClockMaster`).
///
/// Ticked once per audio block, before event delivery, so it emits regardless of
/// whether any inbound MIDI is present this block. Its output goes to its own
/// ring (independent of the unit-keyed routing below), so System Real-Time
/// messages reach hardware-out rather than being dropped by the router.
pub trait BlockClock: Send + Sync {
    /// Generate this block's clock/timecode output. `block_size` is the frame
    /// count of the upcoming audio block.
    fn tick(&self, block_size: usize);
}

const MIDI_EVENT_BUFFER_CAPACITY: usize = 512;

/// Sentinel unit id passed to the pre-routing hardware [`MidiIn`], which ignores
/// it and returns every pending event (routing decides the real targets).
const HARDWARE_POLL_UNIT: MidiUnitId = MidiUnitId::new(0);

/// The once-per-block MIDI producer: polls the hardware input, routes events into
/// unit inboxes, and ticks the outbound clock — all *before* the graph renders.
///
/// Held by the audio-callback assembly alongside the engine; the callback calls
/// [`run`](Self::run) then `engine.process(..)`. Consuming nodes read the
/// `frame_offset` on each delivered event to time it within the block.
pub struct MidiPreBlock {
    /// Hardware / live MIDI source, polled once per block. `None` when no
    /// hardware input is compiled or connected (software fan-out still works —
    /// producers push straight into unit inboxes).
    input: Option<Arc<dyn MidiIn>>,
    /// The fan-out that delivers a routed event to a destination unit's inbox,
    /// keyed by [`MidiUnitId`]. `None` before wiring.
    queue: Option<Arc<dyn MidiRouter>>,
    /// The live routing snapshot: maps each event (by channel) to its target
    /// unit ids. Swapped atomically off-thread.
    routing: Arc<ArcSwap<MidiRoutingSnapshot>>,
    /// `(frame_offset, event)` collected per block. Fixed capacity: events past
    /// [`MIDI_EVENT_BUFFER_CAPACITY`] are dropped (never allocated) on the audio
    /// thread.
    events: RtEventBuf<(usize, MidiEvent), MIDI_EVENT_BUFFER_CAPACITY>,
    /// Scratch the hardware [`MidiIn`] fills each block via `poll_into`, before
    /// we copy into `events`. Interior-mutable so `run` stays `&self` on the
    /// audio path; single-audio-thread access (same contract `events` relies on).
    poll_scratch: AudioThreadCell<[MidiEvent; MIDI_EVENT_BUFFER_CAPACITY]>,
    /// Optional outbound clock/timecode generator, ticked once per block.
    clock: Option<Arc<dyn BlockClock>>,
    /// Input-edge MIDI-1→2 translation, applied to each polled event *before*
    /// routing. `Midi1ToMidi2Translator` assembles multi-message (N)RPN runs
    /// (incl. MPE MCM + pitch-bend-sensitivity); `MpeIngest` rewrites classic-MPE
    /// channel-spread into native per-note messages. Both are stateful and
    /// single-audio-thread, hence `AudioThreadCell`. `None` when the respective
    /// stage is not configured (translation then passes events through).
    translator: AudioThreadCell<Option<Midi1ToMidi2Translator>>,
    mpe: AudioThreadCell<Option<MpeIngest>>,
}

impl MidiPreBlock {
    /// Build a producer reading the given routing snapshot. Input, queue, and
    /// clock are installed separately (they're wired after construction).
    pub fn new(routing: Arc<ArcSwap<MidiRoutingSnapshot>>) -> Self {
        Self {
            input: None,
            queue: None,
            routing,
            events: RtEventBuf::new(),
            poll_scratch: AudioThreadCell::new([MidiEvent::noop(); MIDI_EVENT_BUFFER_CAPACITY]),
            clock: None,
            translator: AudioThreadCell::new(None),
            mpe: AudioThreadCell::new(None),
        }
    }

    /// Install the hardware / live MIDI source polled each block.
    pub fn set_input(&mut self, input: Arc<dyn MidiIn>) {
        self.input = Some(input);
    }

    /// Install the fan-out that delivers routed events to unit inboxes.
    pub fn set_queue(&mut self, queue: Arc<dyn MidiRouter>) {
        self.queue = Some(queue);
    }

    /// Install the per-block clock/timecode generator (see [`BlockClock`]).
    pub fn set_clock(&mut self, clock: Arc<dyn BlockClock>) {
        self.clock = Some(clock);
    }

    /// Install the input-edge (N)RPN assembler. Applied to each polled event
    /// before MPE ingestion + routing, so multi-message RPN runs (MPE MCM,
    /// pitch-bend sensitivity) arrive downstream as single MIDI-2 controller
    /// messages. Off-RT; call at wiring time.
    pub fn set_translator(&mut self, translator: Midi1ToMidi2Translator) {
        *self.translator.borrow_mut() = Some(translator);
    }

    /// Install the classic-MPE → native-per-note ingestion transform for the
    /// configured [`MpeMode`](tutti_midi_types::mpe::MpeMode). Applied after the
    /// (N)RPN assembler, so downstream nodes receive only native per-note MIDI-2.
    /// Off-RT; call at wiring time (and on MPE-mode change).
    pub fn set_mpe_ingest(&mut self, ingest: MpeIngest) {
        *self.mpe.borrow_mut() = Some(ingest);
    }

    /// Run the pre-block MIDI step for a block of `frames` samples.
    ///
    /// Ticks the clock, polls the hardware input, and routes every event into
    /// its destination unit's inbox. Consuming nodes drain those inboxes and
    /// time each event by its `frame_offset` when *they* render. RT-safe:
    /// lock-free, alloc-free.
    #[inline]
    pub fn run(&self, frames: usize) {
        // Tick the outbound clock/timecode generator first — it reads the
        // transport and pushes into its own output ring every block, independent
        // of inbound MIDI.
        if let Some(clock) = &self.clock {
            clock.tick(frames);
        }

        let event_count = self.collect_events(frames);
        if event_count == 0 {
            return;
        }
        // Deliver the whole block at once — each event keeps its `frame_offset`
        // for the destination unit to time it.
        self.route_events();
    }

    /// Reset the interior-mutable RT owner (device switch).
    pub fn reset_owners(&self) {
        self.events.reset_owner();
        self.poll_scratch.reset_owner();
        self.translator.reset_owner();
        self.mpe.reset_owner();
    }

    #[inline]
    fn collect_events(&self, frames: usize) -> usize {
        self.events.clear();

        let Some(input) = &self.input else {
            return 0;
        };

        // Drain the input into scratch, then copy the routed subset into
        // `events` — all inside one `borrow_mut`. `poll_into` ignores the unit id
        // (hardware is pre-routing) and returns everything pending; the copy is
        // bounded and allocation-free. We drain even when nothing is routed, so
        // the hardware rings don't back up.
        let mut scratch = self.poll_scratch.borrow_mut();
        let n = input.poll_into(HARDWARE_POLL_UNIT, frames, &mut scratch[..]);

        let routing = self.routing.load();
        if !routing.has_routes() || n == 0 {
            return 0;
        }

        // Input-edge translation: assemble (N)RPN runs, then rewrite classic-MPE
        // channel-spread into native per-note messages, so downstream nodes see
        // only native MIDI-2. Each stage may absorb an event (returns `None`) or
        // transform it; a `None` stage passes events through unchanged.
        let mut translator = self.translator.borrow_mut();
        let mut mpe = self.mpe.borrow_mut();

        // Capped push reproduces the fixed-budget drop-overflow behaviour; it
        // never allocates on the audio thread.
        for &raw in &scratch[..n] {
            let after_rpn = match translator.as_mut() {
                Some(t) => t.translate(&raw),
                None => Some(raw),
            };
            let Some(ev) = after_rpn else { continue };
            let out = match mpe.as_mut() {
                Some(m) => m.translate(&ev),
                None => Some(ev),
            };
            if let Some(ev) = out {
                let _ = self.events.push((ev.frame_offset as usize, ev));
            }
        }
        self.events.len()
    }

    #[inline]
    fn route_events(&self) {
        let Some(queue) = &self.queue else {
            return;
        };
        let routing = self.routing.load();
        self.events.for_each(|&(_offset, event)| {
            for target in routing.route(&event) {
                queue.queue(target, &[event]);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tutti_midi_types::convert::{midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2};
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2;
    use tutti_midi_types::midi2::UmpMessage;
    use tutti_midi_types::mpe::{MpeMode, MpeZoneConfig};
    use tutti_midi_types::MidiRoutingTable;

    /// A one-shot [`MidiIn`] that returns a fixed event list on its first poll.
    struct FixedInput {
        events: Mutex<Vec<MidiEvent>>,
    }
    impl MidiIn for FixedInput {
        fn poll_into(&self, _unit: MidiUnitId, _block_size: usize, out: &mut [MidiEvent]) -> usize {
            let mut evs = self.events.lock().unwrap();
            let n = evs.len().min(out.len());
            for (slot, ev) in out.iter_mut().zip(evs.drain(..n)) {
                *slot = ev;
            }
            n
        }
    }

    /// Captures every routed event so a test can assert what reached the router.
    #[derive(Default)]
    struct CapturingRouter {
        routed: Mutex<Vec<MidiEvent>>,
    }
    impl MidiRouter for CapturingRouter {
        fn queue(&self, _unit_id: MidiUnitId, events: &[MidiEvent]) {
            self.routed.lock().unwrap().extend_from_slice(events);
        }
    }

    #[test]
    fn classic_mpe_input_arrives_at_router_as_native_per_note() {
        // Route everything to one unit via a fallback.
        let unit = MidiUnitId::new(7);
        let mut table = MidiRoutingTable::new();
        table.set_routes(Vec::new(), Some(unit));
        table.commit();

        // Classic-MPE input: note-on on member channel 2, then a channel pitch
        // bend on ch2 (which classic MPE means "bend note 60 only").
        let note = MidiEvent::note_on(0, 2, 60, midi1_velocity_to_midi2(100));
        let bend = MidiEvent::pitch_bend(0, 2, midi1_pitch_bend_to_midi2(16383));
        let input = Arc::new(FixedInput {
            events: Mutex::new(vec![note, bend]),
        });

        let router = Arc::new(CapturingRouter::default());

        let mut pre = MidiPreBlock::new(table.snapshot_arc());
        pre.set_input(input);
        pre.set_queue(router.clone());
        pre.set_mpe_ingest(MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15))));

        pre.run(256);

        let routed = router.routed.lock().unwrap();
        assert_eq!(routed.len(), 2, "note + bend both routed");
        // First is the note-on (passthrough).
        assert!(routed[0].is_note_on());
        // Second must be a NATIVE per-note pitch bend on note 60, not a channel bend.
        match UmpMessage::try_from(routed[1].data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::PerNotePitchBend(m)) => {
                assert_eq!(u8::from(m.note_number()), 60);
            }
            other => panic!("expected native PerNotePitchBend at the router, got {other:?}"),
        }
    }

    #[test]
    fn no_translation_installed_passes_events_through() {
        let unit = MidiUnitId::new(3);
        let mut table = MidiRoutingTable::new();
        table.set_routes(Vec::new(), Some(unit));
        table.commit();

        let bend = MidiEvent::pitch_bend(0, 2, midi1_pitch_bend_to_midi2(16383));
        let input = Arc::new(FixedInput {
            events: Mutex::new(vec![bend]),
        });
        let router = Arc::new(CapturingRouter::default());

        let mut pre = MidiPreBlock::new(table.snapshot_arc());
        pre.set_input(input);
        pre.set_queue(router.clone());
        // No MpeIngest installed → the channel bend passes straight through.
        pre.run(256);

        let routed = router.routed.lock().unwrap();
        assert_eq!(routed.len(), 1);
        assert!(matches!(
            UmpMessage::try_from(routed[0].data_words()).unwrap(),
            UmpMessage::ChannelVoice2(ChannelVoice2::ChannelPitchBend(_))
        ));
    }
}
