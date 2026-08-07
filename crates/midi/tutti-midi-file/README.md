# tutti-midi-file

MIDI **file** codecs: Standard MIDI File (SMF) and MIDI 2.0 Clip File (M2-116).

## What this is

Reading and writing `.mid` and `.midi2` files. Nothing here touches an OS MIDI
API, and nothing is `cfg`-gated.

## Why it is separate

It used to live in [`tutti-midi-hardware`](../tutti-midi-hardware) — then named
`tutti-midi-io` — behind that crate's
`midi-hardware` feature being *off*. That made "I want to read a `.mid` file" and
"I want to talk to a MIDI port" two settings of one flag, when they are simply
different jobs — and a consumer wanting only the codecs had to know to pass
`default-features = false`, or silently link CoreMIDI.

Splitting turned that flag into a dependency edge. Depend on this crate for
files, on `tutti-midi-hardware` for ports.

## Quick start

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
