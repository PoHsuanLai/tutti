# tutti-midi-file

MIDI **file** codec for the Tutti audio engine: Standard MIDI File (SMF,
`.mid`) and MIDI 2.0 Clip File (M2-116, `.midi2`) read into one model and
written back from it, losslessly. Re-exported by `tutti` as
`tutti::midi_file` (feature `midi`).

## The model

A [`MidiFile`] is its container (an SMF's format 0, 1 or 2, or a Clip File),
its division, its [`Track`]s and the `TempoMap` its tempo events declare. A
track is `(Tick, FileEvent)` in order and where it ends. Ticks are integers,
so a write is exact; [`MidiFile::events`] gives the merged timeline as
[`Beat`](tutti_types::Beat)s, [`MidiFile::timeline`] the same as packets for a clip, and [`Track::notes`] pairs Note Ons with Note
Offs.

- **A [`FileEvent`]** is a `Message` (an SMF's channel message is MIDI 1.0 on
  group 0; a Clip File's packet is as stored), a `SysEx` or `Escape` of any
  length, or a `Meta` (text, tempo, time and key signature, the rest as bytes).
  The file layer does not translate: a MIDI 1.0 Note On at velocity 0 stays one.
- **Lossless.** A read, a write and a read give the same file: SysEx and meta
  included. Running status and a Delta Clockstamp's spelling may differ.
- **Truncation is an error** ([`Error::Truncated`]), not a shorter file.
- **Format 2** keeps its sequences apart: no merged `events()`, a tempo map
  per track ([`MidiFile::tempo_map_of`]).
- **Tempo** is `MidiFile::tempo_map()`, steps at the tick each Set Tempo (an
  SMF's meta event, a Clip File's Flex Data message) lands on; 120 BPM until
  the first if none is at the start. `seconds_at` integrates it.
- **Metrical timing only.** An SMPTE division is refused with
  [`Error::MidiUnsupportedTiming`]: it has no beat grid to put a tempo map on.

- **To a Clip File.** [`MidiFile::to_clip`] converts a format 0 or 1 SMF: its meta
  events become the Flex Data messages of M2-116 Appendix B, and what has no UMP
  form comes back in [`Converted::dropped`], never lost silently.

[`MidiFileKind::detect`] tells the two containers apart by magic, not by
extension. Nothing here touches an OS MIDI API; ports are in
`tutti-midi-hardware`.

## Quick start

Build a file, write it, read it back; the tempo map answers in seconds:

```rust
use tutti_midi_file::{MidiFile, SmfFormat, Tick, Track};
use tutti_types::{Beat, Bpm, Seconds};
use tutti_ump::{MidiChannel, NoteNumber, U7};

// 120 BPM, then 60 from beat 4, at 480 ticks per quarter.
let track = Track::new()
    .tempo(Tick(0), Bpm::of(120.0))
    .note(Tick(0), 1_440, MidiChannel::FIRST, NoteNumber::of(60), U7::of(100))
    .tempo(Tick::quarters(4, 480), Bpm::of(60.0));
let file = MidiFile::smf(SmfFormat::SingleTrack, 480, vec![track])?;

let read = MidiFile::from_bytes(&file.to_bytes()?)?;
assert_eq!(read, file);
assert_eq!(read.seconds_at(Beat(8.0)), Seconds(6.0));
let notes = read.tracks()[0].notes();
assert_eq!((notes[0].start, notes[0].end), (Tick(0), Tick(1_440)));
# Ok::<(), Box<dyn std::error::Error>>(())
```

A clip plays a file's messages as they are: `file.timeline()` is `(Beat, Ump)`
pairs (channel messages as their packets, SysEx as SysEx7 packets, meta events
dropped: the tempo is [`MidiFile::tempo_map`]), which `play_clip` takes
directly.

### Tempo is the wire's

SMF stores tempo as microseconds per quarter note, an integer: `174.0` BPM is
not representable and reads back as `174.0002958...`. That is the format.

### Reading from a path

```rust,no_run
use tutti_midi_file::{MidiFile, MidiFileKind};

// Which container? By magic bytes, not by extension: a `.mid` extension on a
// Clip File is a real thing that happens.
if MidiFileKind::detect_path("song.mid")?.is_some() {
    let file = MidiFile::read("song.mid")?;
    for track in file.tracks() {
        println!("{:?}: {} notes", track.name(), track.notes().len());
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

MIT OR Apache-2.0
