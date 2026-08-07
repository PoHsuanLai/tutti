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

use tutti_midi_types::tutti_types::RtPublish;

use tutti_core::{AudioThreadCell, RtEventBuf};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::Midi1ToMidi2Translator;
use tutti_midi_types::{MidiIn, MidiRouter, MidiRoutingSnapshot};

use crate::outbound::mpe_ingest::MpeIngest;
use tutti_midi_types::mpe::MpeMode;

/// Something ticked once per audio block, before event delivery, whose output
/// **bypasses unit routing**.
///
/// The engine's own implementation is [`ClockMaster`](crate::ClockMaster) —
/// outbound Beat Clock + MTC. This is a trait rather than that concrete type
/// because System Real-Time and timecode are *not addressed to a unit*, so
/// anything generating them needs a slot outside the router, and a consumer's
/// own generator (LTC, a proprietary sync flavour) is as entitled to that slot
/// as ours. It emits regardless of whether inbound MIDI arrived this block.
///
/// Lock-free and alloc-free: audio thread.
pub trait BlockClock: Send + Sync {
    /// Generate this block's clock/timecode output. `block_size` is the frame
    /// count of the upcoming audio block.
    fn tick(&self, block_size: usize);
}

const MIDI_EVENT_BUFFER_CAPACITY: usize = 512;

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
    routing: Arc<RtPublish<MidiRoutingSnapshot>>,
    /// `(frame_offset, event)` collected per block. Fixed capacity: events past
    /// [`MIDI_EVENT_BUFFER_CAPACITY`] are dropped (never allocated) on the audio
    /// thread.
    events: RtEventBuf<(usize, MidiEvent), MIDI_EVENT_BUFFER_CAPACITY>,
    /// Scratch the hardware [`MidiIn`] fills each block via `poll_block`,
    /// before we copy into `events`. Interior-mutable so `run` stays `&self` on
    /// the audio path; single-audio-thread access (same contract `events` relies
    /// on).
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
    /// The MPE mode a control thread wants, published for the audio thread to
    /// adopt at the top of the next block.
    ///
    /// **The mode travels, not the `MpeIngest`.** The ingest holds live
    /// voice-allocation state and `set_mode` resets it wholesale, so it must stay
    /// audio-thread-owned in its `AudioThreadCell` — handing one across threads
    /// would tear that state. An `MpeMode` is a small `Copy` value, which is what
    /// makes it publishable.
    ///
    /// `RtPublish` rather than a bare cell, per the engine's rule for
    /// control→audio state: the audio thread reads a borrow it cannot outlive,
    /// and the retired value is freed on the publisher. `routing` above is the
    /// same pattern for the same reason.
    ///
    /// `None` is the initial state, meaning "the build-time mode stands". Once a
    /// request is published it *stays* published — the audio thread never writes
    /// here, because clearing it would mean publishing from the callback, which
    /// stalls it and frees on it. Re-adoption is harmless instead: the audio side
    /// compares against the mode already in force and does nothing when they
    /// match, which is what keeps a latched request from resetting voice
    /// allocation on every block.
    mpe_request: Arc<RtPublish<Option<MpeMode>>>,
}

impl MidiPreBlock {
    /// Build a producer reading the given routing snapshot. Input, queue, and
    /// clock are installed separately (they're wired after construction).
    pub fn new(routing: Arc<RtPublish<MidiRoutingSnapshot>>) -> Self {
        Self {
            input: None,
            queue: None,
            mpe_request: Arc::new(RtPublish::new(None)),
            routing,
            events: RtEventBuf::new(),
            poll_scratch: AudioThreadCell::new([MidiEvent::noop(); MIDI_EVENT_BUFFER_CAPACITY]),
            clock: None,
            translator: AudioThreadCell::new(None),
            mpe: AudioThreadCell::new(None),
        }
    }

    /// Install the hardware / live MIDI source polled each block.
    /// A [`MidiIn`] and not a [`MidiUnitIn`]: this phase *decides* the
    /// unit ids, so it has none to pass. It used to poll a `MidiIn` with a
    /// `MidiUnitId::new(0)` sentinel — a real id, which meant a per-unit source
    /// installed here would silently have received the entire hardware stream.
    /// The type now refuses that install.
    ///
    /// [`MidiUnitIn`]: tutti_midi_types::MidiUnitIn
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

    /// A handle a control thread can use to change the MPE mode while the engine
    /// runs.
    ///
    /// Cloneable and `Send`, so a host can park it in a resource and write to it
    /// from a normal system — which is what makes MPE configuration *document*
    /// state rather than a build-time constant. The audio thread adopts the
    /// request at the top of the next block; see [`MpeModeRequest::set`].
    pub fn mpe_mode_handle(&self) -> MpeModeRequest {
        MpeModeRequest {
            cell: Arc::clone(&self.mpe_request),
        }
    }

    /// Adopt a published mode request, if one differs from the mode in force.
    /// Audio thread, once per block, before any event is translated.
    ///
    /// **The equality check is load-bearing, not an optimisation.** The request
    /// latches (see [`mpe_request`](Self::mpe_request)), so this runs against the
    /// same value every block; without the check it would call `set_mode` — which
    /// rebuilds the ingest from scratch — on every block, dropping every sounding
    /// note's voice mapping continuously. A mode change is rare; a spurious one
    /// is audible.
    #[inline]
    fn adopt_mpe_request(&self) {
        // One read per block, never per event: the guard is a thread-local
        // lookup plus two `SeqCst` loads, far heavier than the atomics beside it.
        let pending = *self.mpe_request.read();
        let Some(mode) = pending else { return };
        let mut mpe = self.mpe.borrow_mut();
        match mpe.as_mut() {
            Some(ingest) if *ingest.mode() == mode => {}
            Some(ingest) => ingest.set_mode(mode),
            None => *mpe = Some(MpeIngest::new(mode)),
        }
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

        // ONE routing read, taken BEFORE the block is collected and used only by
        // the fan-out below. Both halves of that matter:
        //
        // - *Before*, because `collect_events` polls the input, and a `commit`
        //   racing that poll must not decide this block's delivery. The block
        //   routes by the rules in force when it started
        //   (`a_publish_mid_block_does_not_split_the_block_across_rule_sets`
        //   drives exactly that interleaving, republishing from inside
        //   `poll_into`).
        // - *Only by the fan-out*, because collection must not consult routing at
        //   all — translation is stateful and skipping it corrupts later events.
        //   `collect_events` takes no snapshot parameter, which is what keeps
        //   this from regressing.
        let routing = self.routing.read();

        let event_count = self.collect_events(frames);
        if event_count == 0 {
            return;
        }
        // Deliver the whole block at once — each event keeps its `frame_offset`
        // for the destination unit to time it.
        self.route_events(&routing);
    }

    /// Reset the interior-mutable RT owner (device switch).
    pub fn reset_owners(&self) {
        self.events.reset_owner();
        self.poll_scratch.reset_owner();
        self.translator.reset_owner();
        self.mpe.reset_owner();
    }

    /// Poll the input and translate the block's events into `self.events`.
    ///
    /// **Deliberately takes no routing snapshot.** Collection is an input-edge
    /// concern and consults nothing about delivery; the fan-out in
    /// [`route_events`](Self::route_events) is the sole place routing is
    /// consulted. The absent parameter is what keeps that true — an early return
    /// on an empty table here is precisely the bug this signature forecloses (see
    /// the comment on the poll below).
    #[inline]
    fn collect_events(&self, frames: usize) -> usize {
        self.events.clear();

        let Some(input) = &self.input else {
            return 0;
        };

        // Drain the input into scratch, then copy the routed subset into
        // `events` — all inside one `borrow_mut`. The copy is bounded and
        // allocation-free. We drain even when nothing is routed, so the hardware
        // rings don't back up.
        let mut scratch = self.poll_scratch.borrow_mut();
        let n = input.poll_block(frames, &mut scratch[..]);

        // Only "nothing arrived" short-circuits. Emptiness of the routing table
        // must NOT, even though nothing can be delivered: both stages below are
        // stateful multi-message assemblers, not per-event filters, so skipping
        // them corrupts state for events that arrive *later*, once routes exist.
        //
        // - `Midi1ToMidi2Translator` assembles an (N)RPN run across several CCs;
        //   a run straddling the boundary resumes from a partial parameter number
        //   and names the wrong parameter.
        // - `MpeIngest` holds the member-channel → held-note map. A note-on
        //   skipped here never binds its channel, so the next per-note bend
        //   resolves to no note and `translate` returns `None` — the note sounds
        //   on, deaf to the controller (the same failure the
        //   `re_adopting_a_latched_request_does_not_reset_voice_state` test
        //   guards from a different cause).
        //
        // Translation is an input-edge concern; routing is a delivery concern.
        // Cost of running it unrouted is bounded: allocation-free, capped by
        // `events`' fixed capacity.
        if n == 0 {
            return 0;
        }

        // Input-edge translation: assemble (N)RPN runs, then rewrite classic-MPE
        // channel-spread into native per-note messages, so downstream nodes see
        // only native MIDI-2. Each stage may absorb an event (returns `None`) or
        // transform it; a `None` stage passes events through unchanged.
        // Before any borrow below: adopting takes `self.mpe` mutably, and the
        // cell's contract is one borrow at a time.
        //
        // Adoption is likewise unconditional: `MpeModeRequest::set` documents
        // that "the audio thread adopts the request at the top of the next
        // block", and `pending()` latches — so gating this on routing left the
        // only observable a consumer has reporting a request that never applied.
        self.adopt_mpe_request();

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

    /// Takes the block's routing snapshot from [`run`](Self::run) rather than
    /// reading its own — see `collect_events` for why they must match.
    #[inline]
    fn route_events(&self, routing: &MidiRoutingSnapshot) {
        let Some(queue) = &self.queue else {
            return;
        };
        self.events.for_each(|&(_offset, event)| {
            for target in routing.route(&event) {
                // The accepted count is deliberately dropped: this is the audio
                // thread, one event at a time, and there is no caller to back
                // off. A short count means the destination's 256-slot ring is
                // full — recoverable only by the *consumer* draining faster,
                // which nothing here can influence.
                let _ = queue.queue(target, &[event]);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    /// A latched mode request is adopted once, and re-adoption does not reset
    /// the ingest.
    ///
    /// The request never clears — the audio thread cannot publish, so clearing it
    /// would mean publishing from the callback. Re-adoption is made harmless by
    /// comparing against the mode already in force, and **that comparison is
    /// load-bearing**: `MpeIngest::set_mode` rebuilds the ingest from scratch.
    ///
    /// # Two wrong assertions preceded this one
    ///
    /// Comparing the *mode* proves nothing — a reset leaves it identical.
    /// Comparing a note-off's translation proves nothing either: the `NoteOff`
    /// arm returns `Some(*event)` unconditionally, because a note-off must reach
    /// the synth whether or not the ingest still knows the voice.
    ///
    /// **Per-note pitch bend is what actually observes the voice map.** A member
    /// channel's bend is rewritten into a per-note bend by resolving the held
    /// note, and returns `None` when nothing resolves — so a bend after a reset
    /// vanishes instead of being translated. That is the audible failure: the
    /// note keeps sounding and stops responding to the controller.
    #[test]
    fn re_adopting_a_latched_request_does_not_reset_voice_state() {
        let pre = MidiPreBlock::new(Arc::new(RtPublish::new(MidiRoutingSnapshot::default())));
        let handle = pre.mpe_mode_handle();
        let mode = MpeMode::LowerZone(MpeZoneConfig::lower(6));
        handle.set(mode);

        pre.adopt_mpe_request();
        assert_eq!(
            pre.mpe.borrow().as_ref().map(|i| *i.mode()),
            Some(mode),
            "the request must be adopted"
        );

        // Sound a note on a member channel: this binds the channel to the note.
        let member = MidiChannel::new(1);
        let on = MidiEvent::note_on(MidiGroup::FIRST, member, 60, 0x8000);
        pre.mpe.borrow_mut().as_mut().unwrap().translate(&on);

        // A bend on that member channel resolves the held note while the binding
        // stands — the precondition this test rests on.
        let bend = MidiEvent::pitch_bend(MidiGroup::FIRST, member, 0xC000_0000);
        assert!(
            pre.mpe
                .borrow_mut()
                .as_mut()
                .unwrap()
                .translate(&bend)
                .is_some(),
            "precondition: a bend resolves while the voice is bound"
        );

        // The latched request is still pending; adopting again must be a no-op.
        pre.adopt_mpe_request();

        assert!(
            pre.mpe
                .borrow_mut()
                .as_mut()
                .unwrap()
                .translate(&bend)
                .is_some(),
            "re-adoption reset the voice map — the note is left sounding and \
             deaf to the controller"
        );
    }

    use super::*;
    use std::sync::Mutex;
    use tutti_midi_types::convert::{midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2};
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2;
    use tutti_midi_types::midi2::UmpMessage;
    use tutti_midi_types::mpe::{MpeMode, MpeZoneConfig};
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
    use tutti_midi_types::{MidiRoutingTable, MidiUnitId};

    /// A one-shot [`MidiIn`] that returns a fixed event list on its first
    /// poll.
    struct FixedInput {
        events: Mutex<Vec<MidiEvent>>,
    }
    impl MidiIn for FixedInput {
        fn poll_block(&self, _block_size: usize, out: &mut [MidiEvent]) -> usize {
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
        fn queue(&self, _unit_id: MidiUnitId, events: &[MidiEvent]) -> usize {
            self.routed.lock().unwrap().extend_from_slice(events);
            events.len()
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
        let note = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(2),
            60,
            midi1_velocity_to_midi2(100),
        );
        let bend = MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(2),
            midi1_pitch_bend_to_midi2(16383),
        );
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

    /// A publish that lands *between* the collect gate and the fan-out must not
    /// split the block across two rule sets.
    ///
    /// The window is real but narrow, so rather than race two threads and hope,
    /// this drives it deterministically: the input source republishes the table
    /// from inside `poll_into`, which is exactly the interleaving point — after
    /// `run` takes its snapshot, before the events are routed.
    ///
    /// With one read per block, the whole block routes by the rules in force when
    /// it started. With a read per phase, the gate would consult the old table and
    /// the fan-out the new one, and these events would vanish: the collect phase
    /// admits them (old fallback exists), then the deliver phase drops them (new
    /// table has no routes).
    #[test]
    fn a_publish_mid_block_does_not_split_the_block_across_rule_sets() {
        struct RepublishOnPoll {
            events: Mutex<Vec<MidiEvent>>,
            table: Mutex<MidiRoutingTable>,
        }
        impl MidiIn for RepublishOnPoll {
            fn poll_block(&self, _frames: usize, out: &mut [MidiEvent]) -> usize {
                // Retire every route *while the block is in flight*.
                let mut table = self.table.lock().unwrap();
                table.set_routes(Vec::new(), None);
                table.commit();

                let events = self.events.lock().unwrap();
                let n = events.len().min(out.len());
                out[..n].copy_from_slice(&events[..n]);
                n
            }
        }

        let unit = MidiUnitId::new(11);
        let mut table = MidiRoutingTable::new();
        table.set_routes(Vec::new(), Some(unit));
        table.commit();
        let snapshot = table.snapshot_arc();

        let note = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            midi1_velocity_to_midi2(100),
        );
        let input = Arc::new(RepublishOnPoll {
            events: Mutex::new(vec![note]),
            table: Mutex::new(table),
        });
        let router = Arc::new(CapturingRouter::default());

        let mut pre = MidiPreBlock::new(snapshot);
        pre.set_input(input);
        pre.set_queue(router.clone());

        pre.run(256);

        let routed = router.routed.lock().unwrap();
        assert_eq!(
            routed.len(),
            1,
            "the block must route by the snapshot it started with; a mid-block \
             publish that empties the table must not strand events between the \
             collect gate and the fan-out"
        );
    }

    #[test]
    fn no_translation_installed_passes_events_through() {
        let unit = MidiUnitId::new(3);
        let mut table = MidiRoutingTable::new();
        table.set_routes(Vec::new(), Some(unit));
        table.commit();

        let bend = MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(2),
            midi1_pitch_bend_to_midi2(16383),
        );
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

    /// An empty routing table must not skip MPE ingestion.
    ///
    /// `MpeIngest` is a stateful assembler, not a per-event filter: a note-on
    /// binds its member channel, and later per-note bends resolve against that
    /// binding. Skipping ingestion while nothing is routed therefore does not
    /// merely drop the note-on — it leaves the channel *unbound*, so the first
    /// bend after routing appears resolves to no note, `translate` returns
    /// `None`, and the event never reaches the router. Audibly: the note sounds
    /// on and stops responding to the controller.
    ///
    /// The note-on is deliberately delivered while the table is empty and the
    /// bend only after a route exists — the boundary is the whole point.
    #[test]
    fn a_note_on_received_while_unrouted_still_binds_its_mpe_channel() {
        /// Serves one event per poll, so the two blocks below can straddle a
        /// routing change.
        struct PerBlockInput {
            blocks: Mutex<Vec<Vec<MidiEvent>>>,
        }
        impl MidiIn for PerBlockInput {
            fn poll_block(&self, _frames: usize, out: &mut [MidiEvent]) -> usize {
                let mut blocks = self.blocks.lock().unwrap();
                if blocks.is_empty() {
                    return 0;
                }
                let block = blocks.remove(0);
                let n = block.len().min(out.len());
                out[..n].copy_from_slice(&block[..n]);
                n
            }
        }

        let member = MidiChannel::new(2);
        let note = MidiEvent::note_on(MidiGroup::FIRST, member, 60, midi1_velocity_to_midi2(100));
        let bend =
            MidiEvent::pitch_bend(MidiGroup::FIRST, member, midi1_pitch_bend_to_midi2(16383));

        // Block 1 carries the note-on, block 2 the bend.
        let input = Arc::new(PerBlockInput {
            blocks: Mutex::new(vec![vec![note], vec![bend]]),
        });
        let router = Arc::new(CapturingRouter::default());

        // Start with NO routes at all.
        let mut table = MidiRoutingTable::new();
        table.commit();
        assert!(
            !table.snapshot_arc().read().has_routes(),
            "precondition: the first block runs with an empty routing table"
        );

        let mut pre = MidiPreBlock::new(table.snapshot_arc());
        pre.set_input(input);
        pre.set_queue(router.clone());
        pre.set_mpe_ingest(MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15))));

        // Block 1: unrouted. Nothing can be delivered — but the note-on must
        // still be ingested so the member channel is bound.
        pre.run(256);
        assert_eq!(
            router.routed.lock().unwrap().len(),
            0,
            "nothing is routed while the table is empty"
        );

        // A route now appears.
        table.set_routes(Vec::new(), Some(MidiUnitId::new(9)));
        table.commit();

        // Block 2: the bend must resolve against the binding made in block 1.
        pre.run(256);

        let routed = router.routed.lock().unwrap();
        assert_eq!(
            routed.len(),
            1,
            "the bend vanished: the note-on received while unrouted never bound \
             its channel, so it resolved to no note — the note is left sounding \
             and deaf to the controller"
        );
        match UmpMessage::try_from(routed[0].data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::PerNotePitchBend(m)) => {
                assert_eq!(
                    u8::from(m.note_number()),
                    60,
                    "the bend resolved to the wrong note"
                );
            }
            other => panic!("expected a native PerNotePitchBend for note 60, got {other:?}"),
        }
    }

    /// A latched mode request must be adopted even while nothing is routed.
    ///
    /// `MpeModeRequest::set` documents that the audio thread adopts at the top of
    /// the next block, and `pending()` latches by design — so if adoption sits
    /// behind a routing check, the only observable a consumer has keeps reporting
    /// a request that has never applied.
    #[test]
    fn an_mpe_mode_request_is_adopted_while_unrouted() {
        let mut table = MidiRoutingTable::new();
        table.commit();

        // One event, so the block has something to translate; it goes nowhere.
        let note = MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(2),
            60,
            midi1_velocity_to_midi2(100),
        );
        let input = Arc::new(FixedInput {
            events: Mutex::new(vec![note]),
        });

        let mut pre = MidiPreBlock::new(table.snapshot_arc());
        pre.set_input(input);

        let mode = MpeMode::LowerZone(MpeZoneConfig::lower(6));
        pre.mpe_mode_handle().set(mode);

        pre.run(256);

        assert_eq!(
            pre.mpe.borrow().as_ref().map(|i| *i.mode()),
            Some(mode),
            "the request was never adopted, yet `pending()` still reports it as \
             standing — the consumer has no way to learn it did not apply"
        );
    }
}

/// A control-thread handle for changing the MPE mode while the engine runs.
///
/// Obtained from [`MidiPreBlock::mpe_mode_handle`]. Cloneable and `Send`, so a
/// host parks one in a resource and writes to it from an ordinary system — which
/// is what lets MPE zone configuration live in a *document* rather than being
/// fixed when the engine is built.
///
/// The audio thread adopts a request at the top of the next block. Nothing here
/// blocks, and nothing reaches into audio-thread state.
#[derive(Clone)]
pub struct MpeModeRequest {
    cell: Arc<RtPublish<Option<MpeMode>>>,
}

impl MpeModeRequest {
    /// Ask the engine to switch to `mode` on the next block.
    ///
    /// **Control thread only** — `publish` blocks until in-flight readers are
    /// done and frees the retired value on the calling thread, both of which are
    /// forbidden inside an audio callback.
    ///
    /// Changing the mode **resets MPE voice allocation**: `MpeIngest::set_mode`
    /// rebuilds the ingest, so any note sounding through a member channel loses
    /// its mapping. That is inherent to changing zone layout mid-performance, not
    /// an artefact of this path — but it is why the audio side skips a request
    /// that matches the mode already in force, and why a caller should not write
    /// this every frame.
    pub fn set(&self, mode: MpeMode) {
        self.cell.publish(Arc::new(Some(mode)));
    }

    /// The last mode requested through this handle, if any.
    ///
    /// Diagnostic rather than a source of truth. It **latches** — the audio
    /// thread never clears it — so it answers "what was last asked for", not
    /// "what mode is the engine in". Those differ for one block after a `set`,
    /// and forever if the engine was never started.
    pub fn pending(&self) -> Option<MpeMode> {
        *self.cell.read()
    }
}
