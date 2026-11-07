# tutti-rustysynth

`tutti-rustysynth` is tutti's SoundFont synthesizer: a port of Nobuaki Tanaka's
[rustysynth](https://github.com/sinshu/rustysynth) (itself ported from
[MeltySynth](https://github.com/sinshu/meltysynth)), kept sample for sample
with upstream 1.3.6 by a test in `tutti-soundfont`. The SoundFont file is
`tutti-sf2`'s; `tutti-soundfont` is this crate's one dependent.

* Suitable for both real-time and offline synthesis.
* MIDI messages are typed: `MidiChannel`, `NoteNumber`, `CCNumber`, `U7` and
  `U14` from `tutti-ump`, and the rate is a `SampleRate`.
* No `unsafe`, and no standard MIDI file support: a MIDI file is
  `tutti-midi-file`'s.

```no_run
use std::fs::File;
use std::sync::Arc;
use tutti_rustysynth::{Synthesizer, SynthesizerSettings};
use tutti_sf2::SoundFont;
use tutti_types::SampleRate;
use tutti_ump::{MidiChannel, NoteNumber, U7};

// Load the SoundFont and create the synthesizer.
let sound_font = Arc::new(SoundFont::new(&mut File::open("TimGM6mb.sf2")?)?);
let settings = SynthesizerSettings::new(SampleRate::SR_44K1);
let mut synthesizer = Synthesizer::new(&sound_font, &settings)?;

// Play middle C.
let key = NoteNumber::new(60).unwrap();
synthesizer.note_on(MidiChannel::FIRST, key, U7::new(100).unwrap());

// Render a second of audio.
let mut left = vec![0.0; 44_100];
let mut right = vec![0.0; 44_100];
synthesizer.render(&mut left, &mut right);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

This crate is available under [the MIT license](LICENSE.txt); the copyright is the original author's.
