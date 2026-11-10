# tutti-sf2

`tutti-sf2` reads a SoundFont 2 (`.sf2`) file into data: the presets, the
instruments, the sample headers and the 16-bit sample data. It holds nothing
that plays; `tutti-soundfont` is the player over it.

The code is the file-reading half of Nobuaki Tanaka's
[rustysynth](https://github.com/sinshu/rustysynth) (ported from
[MeltySynth](https://github.com/sinshu/meltysynth)); see [the license](LICENSE.txt).

Generator values stay raw, in the unit the SoundFont 2.04 specification gives
each (`GeneratorType`); turning a value into seconds or hertz is the player's.
A file with a compressed (SoundFont 3) `smpl` chunk is refused with
`Error::UnsupportedSampleFormat`; the `sm24` chunk and the modulator chunks are
skipped.

Sample positions are `tutti_types::Samples`, a rate is a `SampleRate` and a
pitch correction `Cents`. A preset's bank is a `Bank` (a MIDI bank 0 to 127, or
percussion) and its program a `U7`; a preset whose file values are neither
cannot be played but is kept, as the specification says (section 7.2), and
reports `None`. An exclusive class is an `Option<NonZeroU8>`: `None` is class 0.
The crate's failure type is `Error`, with a `Result` alias.

```no_run
use std::fs::File;
use tutti_sf2::SoundFont;

let soundfont = SoundFont::new(&mut File::open("piano.sf2")?)?;
for preset in soundfont.presets() {
    if let (Some(bank), Some(patch)) = (preset.bank(), preset.patch()) {
        println!("{bank}:{} {}", patch.get(), preset.name());
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

[MIT](LICENSE.txt); the copyright is the original author's.
