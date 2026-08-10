# tutti-midi-file

MIDI **file** codecs: Standard MIDI File (SMF) and MIDI 2.0 Clip File (M2-116).

## What this is

Reading and writing `.mid` and `.midi2` files. Nothing here touches an OS MIDI
API, and nothing is `cfg`-gated.

## Why it is its own crate

Reading a `.mid` file and talking to a MIDI port are different jobs, and this
crate is the boundary that keeps them apart. Depend on this one for files, on
[`tutti-midi-hardware`](../tutti-midi-hardware) for ports.

The consequence worth stating: **a consumer that only reads files never links an
OS MIDI API.** No CoreMIDI on macOS, no ALSA sequencer on Linux. That is a
dependency edge rather than a feature flag, so it cannot be got wrong by
forgetting `default-features = false`.

## Quick start

The codec works on **byte slices**, so a round trip needs no file at all. Write
a note, read it back as a paired note positioned in `Beat` and measured in
`BeatDuration` — the same `tutti-core` vocabulary the transport uses, which is
what lets an imported clip be placed without a conversion step:

```rust
use tutti_core::{Beat, BeatDuration};
use tutti_midi_file::{
    encode_midi_file, smf, MidiFileKind, MidiWriteOptions, SmfMessage, SmfTimedEvent,
};

let note = |beat, msg| SmfTimedEvent { time_beats: Beat(beat), channel: 0, msg };
let track = vec![
    note(0.0, SmfMessage::NoteOn { key: 60.into(), vel: 100.into() }),
    note(1.5, SmfMessage::NoteOff { key: 60.into(), vel: 0.into() }),
];

let bytes = encode_midi_file(
    &[track],
    &MidiWriteOptions { ticks_per_beat: 480, tempo_bpm: Some(174.0), ..Default::default() },
)?;

// Recognised by magic bytes rather than by extension.
assert_eq!(MidiFileKind::sniff(&bytes), Some(MidiFileKind::StandardMidiFile));

// Note-on/off paired into whole notes, one per track.
let tracks = smf::tracks(&bytes)?;
assert_eq!(tracks[0].notes.len(), 1);
assert_eq!(tracks[0].notes[0].start_beats, Beat(0.0));
assert_eq!(tracks[0].notes[0].duration_beats, BeatDuration(1.5));
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Tempo does not survive the round trip exactly, and that is the format

SMF stores tempo as **microseconds per quarter note**, an integer. `174.0` BPM
is not representable, so it comes back as `174.0002958…` — a wire quantisation,
not a precision bug. It is read back as `f64` for the same reason SMPTE timecode
is: `Seconds` is `f32`, which cannot carry a long render duration or a timecode
position, so this crate's time-of-day quantities deliberately stay `f64`.

```rust
# use tutti_midi_file::{encode_midi_file, MidiWriteOptions, ParsedMidiFile, SmfMessage, SmfTimedEvent};
# use tutti_core::Beat;
# let track = vec![SmfTimedEvent {
#     time_beats: Beat(0.0),
#     channel: 0,
#     msg: SmfMessage::NoteOn { key: 60.into(), vel: 100.into() },
# }];
let bytes = encode_midi_file(
    &[track],
    &MidiWriteOptions { tempo_bpm: Some(174.0), ..Default::default() },
)?;
// `tempo_bpm` is a plain `f64` — a file with no tempo event reads back as 120.
let parsed = ParsedMidiFile::parse(&bytes)?;
assert_ne!(parsed.tempo_bpm, 174.0);
assert!((parsed.tempo_bpm - 174.0).abs() < 0.001);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Reading from a path

Sniffing a real file needs one to exist, so this half is `no_run`:

```rust,no_run
use tutti_midi_file::{smf, MidiFileKind};

// Which format is this? By magic bytes, not by extension — a `.mid` extension
// on a Clip File is a real thing that happens.
match MidiFileKind::sniff_path("song.mid")? {
    Some(MidiFileKind::StandardMidiFile) => {
        // Per-track paired notes, positioned in beats.
        for track in smf::tracks_from_path("song.mid")? {
            println!("{:?}: {} notes", track.name, track.notes.len());
        }
    }
    Some(MidiFileKind::ClipFile) => {
        let clip = tutti_midi_file::read_clip_file_from_path("song.mid")?;
        println!("{} events", clip.events.len());
    }
    None => println!("not a MIDI file"),
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Constraints worth knowing

- **Metrical timing only.** An SMF using SMPTE/timecode division returns
  `Error::MidiUnsupportedTiming` — beats come from the file's division, and a
  timecode file has no beat grid to read them from.
- **The tempo map is not applied.** Beat positions are as the file states them.

## License

MIT OR Apache-2.0
