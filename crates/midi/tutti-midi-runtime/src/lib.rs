//! MIDI runtime state machines for the Tutti audio engine.
//!
//! Pure MIDI types live in [`tutti_midi_types`]. Hardware I/O lives in
//! `tutti-midi-hardware`. This crate owns the *runtime state* that connects them:
//!
//! - [`MidiMailbox`] / [`MidiSender`] / [`MidiReceiver`] — lock-free
//!   per-unit MIDI inboxes; nodes own a receiver, callers push via senders
//! - [`MidiBus`] — fan-out mapping [`tutti_midi_types::MidiUnitId`] to
//!   [`MidiSender`], dispatching queued events to the right inbox; the
//!   `tutti` engine installs one as its audio-thread dispatch target
//! - [`MidiSnapshot`] — non-destructive event storage for offline export
//! - [`MidiSnapshotReader`] — [`tutti_midi_types::MidiUnitIn`] implementation that
//!   reads a snapshot on an offline timeline
//! - [`MidiRoutingTable`] — UI-thread writer for routing rules, publishing
//!   immutable [`tutti_midi_types::MidiRoutingSnapshot`] values via `RtPublish`
//! - Engine-produced MIDI *out* (e.g. the [`ClockMaster`]'s Beat Clock / MTC)
//!   rides the *same* [`MidiMailbox`] mailbox as MIDI in: the producer holds a
//!   [`MidiSender`] (lock-free `&self` push via [`tutti_midi_types::MidiOut`]),
//!   an off-RT pump drains the paired [`MidiReceiver`] to a hardware-out port
//! - [`MpeIngest`] — input-edge transform rewriting classic-MPE channel-spread
//!   into native MIDI-2 per-note messages (per M2-104, MPE is an ingestion
//!   concern; synth voices track per-note expression themselves)
//!
//! # Example: an event reaching a unit's inbox
//!
//! A [`MidiBus`] routes by [`MidiUnitId`](tutti_midi_types::MidiUnitId); the
//! unit owns the paired [`MidiReceiver`] and drains it at the top of its block.
//! Nothing here allocates or locks, which is what lets the poll side sit on the
//! audio thread.
//!
//! ```
//! use tutti_midi_runtime::{MidiBus, MidiMailbox};
//! use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
//! use tutti_midi_types::{MidiEvent, MidiUnitId};
//!
//! let synth = MidiUnitId::new(7);
//! let (tx, rx) = MidiMailbox::pair(synth);
//!
//! let bus = MidiBus::new();
//! bus.insert(tx);
//!
//! let note = MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100);
//! assert_eq!(bus.queue(synth, &[note]), 1);
//!
//! // The unit's side of the block boundary.
//! let mut block = [MidiEvent::noop(); 8];
//! assert_eq!(rx.poll_into(&mut block), 1);
//! assert_eq!(block[0].note(), Some(60));
//! ```
//!
//! # Example: MIDI on the engine's timeline
//!
//! The seam with `tutti-core` is the transport. A [`MidiSnapshot`] stores events
//! at absolute [`Beat`](tutti_core::Beat)s; a [`MidiSnapshotReader`] emits the
//! ones the block just crossed, stamped with a sample-accurate `frame_offset`.
//! A poll that advanced no beats yields nothing — the window is half-open, so
//! no event is emitted twice.
//!
//! ```
//! use std::sync::Arc;
//! use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig};
//! use tutti_core::{Beat, Bpm, SampleRate};
//! use tutti_midi_runtime::{MidiSnapshot, MidiSnapshotReader};
//! use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
//! use tutti_midi_types::{MidiEvent, MidiUnitId, MidiUnitIn};
//!
//! let synth = MidiUnitId::new(7);
//! let mut snapshot = MidiSnapshot::new();
//! snapshot.add_event(
//!     synth,
//!     Beat(0.0),
//!     MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100),
//! );
//!
//! let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
//!     start_beat: Beat(0.0),
//!     tempo: Bpm(120.0),
//!     sample_rate: SampleRate(48_000.0),
//!     loop_range: None,
//! }));
//! let reader = MidiSnapshotReader::new(snapshot, Arc::clone(&timeline));
//!
//! let mut block = [MidiEvent::noop(); 8];
//! assert_eq!(reader.poll_unit(synth, 512, &mut block), 0); // no beats crossed yet
//!
//! timeline.advance(24_000); // half a beat at 120 BPM / 48 kHz
//! assert_eq!(reader.poll_unit(synth, 512, &mut block), 1);
//! assert_eq!(block[0].note(), Some(60));
//! ```
//!
//! # Example: MPE folded to native per-note messages
//!
//! [`MpeIngest`] is the input edge: a member channel's bend is rewritten as a
//! *per-note* bend addressed at the note that channel holds, so no voice
//! downstream has to know what MPE is.
//!
//! ```
//! use tutti_midi_runtime::MpeIngest;
//! use tutti_midi_types::mpe::{MpeMode, MpeZoneConfig};
//! use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
//! use tutti_midi_types::MidiEvent;
//!
//! let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(7)));
//!
//! // A note-on claims member channel 1; the map now resolves that channel.
//! let member = MidiChannel::new(1);
//! ingest
//!     .translate(&MidiEvent::note_on_7bit(MidiGroup::FIRST, member, 60, 100))
//!     .expect("a note-on passes through");
//!
//! // A channel-wide bend on that member becomes a per-note bend on note 60.
//! let bend = MidiEvent::pitch_bend(MidiGroup::FIRST, member, 0xC000_0000);
//! let folded = ingest.translate(&bend).expect("the channel holds a note");
//! assert_eq!(folded.note(), Some(60));
//! ```
//!
//! **[`MpeIngest::set_mode`] drops all held-note state.** The channel→note maps
//! are keyed by a channel range the new mode may not share, so they cannot carry
//! over. A note held across the call is forgotten: its note-off arrives on a
//! channel with no mapping and passes through *unfolded*. Silence the sounding
//! voices separately — reconfiguring is not a panic.

pub use tutti_midi_types;

pub mod block;
pub mod negotiate;
pub mod outbound;
pub mod schedule;
pub mod sysex;

pub use block::{
    BlockClock, MidiBus, MidiInPort, MidiMailbox, MidiOutSink, MidiPostBlock, MidiPreBlock,
    MidiReceiver, MidiSender, MpeModeRequest, MIDI_OUT_LATENCY_BLOCKS,
};
pub use negotiate::{
    CiInitiator, CiProperty, CiResponder, DeviceIdentity, DiscoveredCiDevice, DiscoveredEndpoint,
    EndpointInquiry, EndpointNegotiator, FunctionBlock,
};
pub use outbound::{
    ClockMaster, JrClock, JrClockEmitter, JrReceiver, JrStamper, JrStream, MpeIngest,
    JR_CLOCK_INTERVAL, JR_CLOCK_MAX_INTERVAL,
};
pub use schedule::{
    MidiClipSource, MidiSnapshot, MidiSnapshotReader, TimedClipEvent, TimedMidiEvent,
};
pub use sysex::{Sysex7PacketReassembler, Sysex8Abort, Sysex8Event, Sysex8PacketReassembler};

// Value types that live one crate down, re-exported so a consumer of the runtime
// needs one import rather than two.
//
// `MidiRoutingTable` sits in `tutti-midi-types` beside the immutable
// `MidiRoutingSnapshot` it publishes — the mutable and published halves of one
// concept. It reached consumers through a local `routing_table` module that did
// nothing but re-export it; the module's only reference repo-wide was its own
// doc comment, so it is gone and this is direct.
//
// The MPE mode/zone types are here for the same reason. The runtime's own MPE
// state machine is not — MPE is an input-edge transform now, `MpeIngest`.
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};
pub use tutti_midi_types::MidiRoutingTable;
