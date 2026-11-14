//! Every piece of the session's MIDI in one program, offline and with no device.
//!
//! It builds a session, adds a synth as an instrument, sets MPE once, builds a
//! MIDI file and plays it as a clip, sends live notes, routes the hardware
//! input by Group, records a take from the live input and drains it, reads the
//! session's MIDI events, lists the devices, and handles the one MIDI error.
//! It needs no hardware: a session with no OS MIDI layer lists no devices and
//! refuses a connection with [`midi::Error::NoHardware`](tutti::midi::Error),
//! which the program prints and goes on.
//!
//! ```text
//! cargo run -p tutti --features midi-hardware,synth --example midi_all
//! ```
//!
//! CI runs it as a test (`test = true`).
//!
//! **Not covered here:** MIDI Time Code out, and the MIDI-CI session
//! ([`Midi::enable_ci`](tutti::Midi::enable_ci) and
//! [`Midi::ci_peers`](tutti::Midi::ci_peers)), which needs a peer device.

use tutti::midi::{self, MidiRouteRule, TakePlacement};
use tutti::midi_file::{FileEvent, SmfFormat};
use tutti::midi_protocol::{MpeMode, MpeZoneConfig, MpeZones};
use tutti::polysynth::{PolySynth, SynthConfig};
use tutti::ump::U7;
use tutti::{
    AddedInstrument, Beat, ChannelLayout, Error, EventCapacity, Frame, InterleavedMut, MaxBlock,
    Message, MidiChannel, MidiFile, MidiGroup, NoteNumber, NoteVelocity, SampleRate, Samples,
    Session, SessionEvent, Stamp, Tick, Track,
};
use tutti_ump::Voice1;

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 512;
const TPQ: u16 = 480;

/// What the run found, for the test below to hold it to.
struct Outcome {
    /// The loudest sample rendered.
    peak: f32,
    /// What the take read from the live input, with where each was placed.
    take: Vec<(Frame, Frame)>,
    /// The session's MIDI events, as their debug text.
    events: Vec<String>,
    /// The devices listed, and whether connecting an absent one was refused as
    /// the session has no OS layer or no such device.
    devices: usize,
    refused: bool,
}

/// A session for the program: the hardware's OS layer if it opens, else none.
/// Hears Groups 1 and 2 (`midi_groups`), so a route can name Group 2.
fn session() -> Result<Session, Error> {
    let build = |hardware: bool| {
        let builder = Session::builder().midi_groups(MidiGroup::of(1));
        let builder = if hardware {
            builder.midi_hardware()
        } else {
            builder
        };
        builder.build_offline(RATE, ChannelLayout::STEREO, MaxBlock::of(Samples(BLOCK)))
    };
    // A machine with no MIDI sequencer fails to open the OS layer: say so, and
    // run on without it.
    build(true).or_else(|e| {
        println!("no OS MIDI layer ({e}); running without devices");
        build(false)
    })
}

/// A file built from a `Track`: four quarter notes up a scale, collected from
/// `(Tick, event)` pairs, then written and read back as bytes (an SMF).
fn file() -> Result<MidiFile, Box<dyn std::error::Error>> {
    let track: Track = (0..4u8)
        .flat_map(|n| {
            let at = Tick::quarters(n.into(), TPQ);
            let key = NoteNumber::of(60 + 2 * n);
            let (on, off): (FileEvent, FileEvent) = (
                Message::midi1(MidiChannel::FIRST, Voice1::note_on(key, U7::of(100))).into(),
                Message::midi1(MidiChannel::FIRST, Voice1::note_off(key, U7::of(64))).into(),
            );
            [(at, on), (at + 240, off)]
        })
        .collect();
    let built = MidiFile::smf(SmfFormat::SingleTrack, TPQ, vec![track])?;
    Ok(MidiFile::from_bytes(&built.to_bytes()?)?)
}

fn run() -> Result<Outcome, Box<dyn std::error::Error>> {
    let mut s = session()?;

    // MPE is set once, on the session: the ingest reads it, and a synth added
    // after is built in it (the session's mode wins over the synth's own
    // `SynthConfig::mpe`).
    let zone = MpeZones::new(MpeZoneConfig::lower(7), None).ok_or("one lower zone")?;
    s.midi().set_mpe(MpeMode::Zones(zone));

    // A synth as an instrument: its strip is wired to the master.
    let AddedInstrument {
        handle: mut synth, ..
    } = s.add_instrument(PolySynth::new(SynthConfig::default()))?;

    // A take: an event tap on what the instrument's handle plays, read with `drain`.
    let mut take = s.midi().record(synth.node(), EventCapacity::of(256))?;

    // A clip from a file's timeline: the packets it holds at their beats.
    let file = file()?;
    s.midi().play_file(synth.node(), Beat(0.0), &file)?;

    // Live notes, through the handle's keys: a MIDI 2.0 note with its
    // 16-bit velocity, and a decoded `Message`, which `push` takes as it takes
    // a packet.
    let live = synth.keys_mut();
    live.note_on(
        Stamp::NextBlock,
        MidiChannel::of(1),
        NoteNumber::of(72),
        NoteVelocity::new(0xC000),
    )?;
    live.push(
        Stamp::NextBlock,
        Message::midi1(
            MidiChannel::of(1),
            Voice1::note_on(NoteNumber::of(76), U7::of(90)),
        ),
    )?;

    // The hardware input, routed by Group: Group 2's channel 3 to the synth. A
    // rule is an id for `unroute`; nothing arrives with no device, and the
    // call refuses only on a session built with no input node.
    let rule = MidiRouteRule::for_channel(MidiChannel::of(3))
        .in_group(MidiGroup::of(1))
        .to(synth.node());
    let route = s.midi().route(rule)?;

    // Devices, behind a check that there are any. Connecting one that is not
    // there is the one MIDI error, with its cause as the source.
    let devices = list_devices(&mut s);
    let refused = match s.midi().connect_input("No Such Keyboard") {
        Err(midi::Error::NoHardware) => {
            println!("no OS MIDI layer: a connection is refused");
            true
        }
        Err(midi::Error::Hardware(e)) => {
            println!("no such device: {e}");
            true
        }
        Err(e) => return Err(e.into()), // any other MIDI failure is the session's `Error::Midi`
        Ok(()) => false,
    };
    // A device saved by an earlier run comes back through its `DeviceInfo`; only
    // an exact match is connected, the rest are for the app to ask about:
    //
    //     match s.midi().reconnect_saved(&saved)? {
    //         Resolution::Connected(id) => ..,
    //         Resolution::SameName(info) => ask(info),
    //         Resolution::Ambiguous(candidates) => ask_among(candidates),
    //         Resolution::Missing => ..,
    //     }
    s.midi().unroute(route)?;

    // Play, render, and read what the session reports.
    s.play()?;
    let mut events = Vec::new();
    let mut peak = 0.0f32;
    let mut block = vec![0.0f32; BLOCK * 2];
    for _ in 0..200 {
        for event in s.update() {
            match event {
                SessionEvent::Midi(e) => events.push(format!("{e:?}")),
                other => events.push(format!("{other:?}")),
            }
        }
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO));
        peak = block.iter().fold(peak, |p, x| p.max(x.abs()));
    }

    // The take: what was played, each event on its frame of the graph, placed
    // where the performer meant it for this device's latencies (figures a host
    // reads off its devices; here zero, and a 10 ms trim).
    let placement = TakePlacement {
        trim: Samples(480),
        ..TakePlacement::default()
    };
    let take = take
        .drain()
        .map(|e| (e.frame, placement.place(e.frame)))
        .collect();
    Ok(Outcome {
        peak,
        take,
        events,
        devices,
        refused,
    })
}

/// The devices present, inputs and outputs. Empty with no OS layer.
fn list_devices(s: &mut Session) -> usize {
    let devices = s.midi().devices();
    for d in &devices {
        println!("device {:?}: {} ({:?})", d.id, d.name, d.direction);
    }
    println!("{} MIDI device(s)", devices.len());
    devices.len()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let o = run()?;
    println!("peak {:.3}", o.peak);
    println!("take: {} events", o.take.len());
    for (frame, placed) in &o.take {
        println!("  played at frame {frame}, placed at {placed}");
    }
    for e in &o.events {
        println!("session event: {e}");
    }
    let _ = (o.devices, o.refused);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The program runs offline and hears what it played**: the clip and the
    /// live notes sound (a peak), the take holds the two live notes on their
    /// frames and each is placed 480 frames later (the trim), a connection to a
    /// device that is not there is refused, and no device is listed on a runner
    /// with none (a runner that has some lists them, so only the count of what
    /// the session saw is held to its own list).
    #[test]
    fn the_program_runs_and_hears_what_it_played() {
        let o = run().expect("the program runs");
        assert!(o.peak > 0.01, "the synth sounded: {}", o.peak);
        assert_eq!(o.take.len(), 2, "the two live notes: {:?}", o.take);
        for (frame, placed) in &o.take {
            assert_eq!(placed.get(), frame.get() + 480, "the trim moves it later");
        }
        assert!(o.refused, "a keyboard that is not there is refused");
        assert!(
            o.events.iter().all(|e| !e.contains("EventsDropped")),
            "nothing dropped: {:?}",
            o.events
        );
    }
}
