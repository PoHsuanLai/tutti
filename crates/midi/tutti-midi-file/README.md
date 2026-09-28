# tutti-midi-file

MIDI **file** codecs for the Tutti audio engine: Standard MIDI File ([`smf`],
`.mid`) and MIDI 2.0 Clip File ([`clip`], M2-116, `.midi2`). Re-exported by
`tutti` as `tutti::midi_file` (feature `midi`).

## What the two codecs cover

- [`smf`]: SMF 1.0. Parse to beat-positioned events ([`ParsedMidiFile`]) or
  per-track paired notes ([`smf::tracks`]), and write ([`encode_midi_file`] /
  [`write_midi_file`]). Metrical timing only; the tempo map is reported, not
  applied.
- [`clip`]: the *file-level* (path) half of the MIDI 2.0 Clip File codec
  ([`read_clip_file_from_path`], [`write_clip_file_to_path`]). The byte-level
  codec is in `tutti-midi-types` and is re-exported here ([`read_clip_file`],
  [`write_clip_file`]), so a clip round-trips through a path with one import.

[`MidiFileKind::sniff`] tells the two formats apart by magic bytes, not by
extension. Errors are [`Error`](enum@Error).

Nothing here touches an OS MIDI API, and nothing is `cfg`-gated, so a consumer
that only reads files never links CoreMIDI or the ALSA sequencer. Ports are in
`tutti-midi-hardware`, which does not re-export these codecs.

## Quick start

The codec works on **byte slices**, so a round trip needs no file at all. Write
a note, read it back as a paired note positioned in `Beat` and measured in
`BeatDuration` — the same `tutti-core` vocabulary the transport uses, which is
what lets an imported clip be placed without a conversion step:

```rust
use tutti_core::{Beat, BeatDuration, Bpm};
use tutti_midi_file::{
    encode_midi_file, smf, MidiFileKind, MidiWriteConfig, SmfMessage, SmfTimedEvent,
};

let note = |beat, msg| SmfTimedEvent { time_beats: Beat(beat), channel: 0, msg };
let track = vec![
    note(0.0, SmfMessage::NoteOn { key: 60.into(), vel: 100.into() }),
    note(1.5, SmfMessage::NoteOff { key: 60.into(), vel: 0.into() }),
];

let bytes = encode_midi_file(
    &[track],
    &MidiWriteConfig { ticks_per_beat: 480, tempo_bpm: Some(Bpm(174.0)), ..Default::default() },
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
is not representable, so it comes back as `174.0002958…`: a wire quantisation,
not a precision bug. Tempo is read back as `Bpm` (f64-backed, so nothing more is
lost after the wire).

```rust
# use tutti_midi_file::{encode_midi_file, MidiWriteConfig, ParsedMidiFile, SmfMessage, SmfTimedEvent};
# use tutti_core::{Beat, Bpm};
# let track = vec![SmfTimedEvent {
#     time_beats: Beat(0.0),
#     channel: 0,
#     msg: SmfMessage::NoteOn { key: 60.into(), vel: 100.into() },
# }];
let bytes = encode_midi_file(
    &[track],
    &MidiWriteConfig { tempo_bpm: Some(Bpm(174.0)), ..Default::default() },
)?;
// A file with no tempo event reads back as the SMF default, `Bpm(120.0)`.
let parsed = ParsedMidiFile::parse(&bytes)?;
assert_ne!(parsed.tempo_bpm, Bpm(174.0));
assert!(!parsed.tempo_bpm.differs_from(Bpm(174.0), 0.001));
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
  [`Error::MidiUnsupportedTiming`] — beats come from the file's division, and a
  timecode file has no beat grid to read them from.
- **The tempo map is not applied.** Beat positions are as the file states them.

## License

MIT OR Apache-2.0
