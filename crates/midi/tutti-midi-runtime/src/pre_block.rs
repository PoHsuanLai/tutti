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
//! 3. routes each event to its destination unit's inbox via the [`MidiRouter`]
//!    fan-out — each event keeps its own `frame_offset`, so the consuming node
//!    times it correctly.
//!
//! Every method is **lock-free** and **alloc-free**, safe on the audio thread.

use std::sync::Arc;

use arc_swap::ArcSwap;

use tutti_core::{AudioThreadCell, RtEventBuf};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiIn, MidiRouter, MidiRoutingSnapshot, MidiUnitId};

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

        // Capped push reproduces the fixed-budget drop-overflow behaviour; it
        // never allocates on the audio thread.
        for &event in &scratch[..n] {
            let _ = self.events.push((event.frame_offset as usize, event));
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
