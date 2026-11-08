# tutti-sf2

`tutti-sf2` reads a SoundFont 2 (`.sf2`) file into data: the presets, the
instruments, the sample headers and the 16-bit sample data. It holds nothing
that plays; `tutti-soundfont` is the player over it.

The code is the file-reading half of Nobuaki Tanaka's
[rustysynth](https://github.com/sinshu/rustysynth) (ported from
[MeltySynth](https://github.com/sinshu/meltysynth)); see [the license](LICENSE.txt).

Generator values stay raw, in the unit the SoundFont 2.04 specification gives
each (`GeneratorType`). A file with a compressed (SoundFont 3) `smpl` chunk is
refused; the `sm24` chunk and the modulator chunks are skipped.

```no_run
use std::fs::File;
use tutti_sf2::SoundFont;

let soundfont = SoundFont::new(&mut File::open("piano.sf2")?)?;
for preset in soundfont.presets() {
    println!("{}:{} {}", preset.bank_number(), preset.patch_number(), preset.name());
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

## License

[MIT](LICENSE.txt); the copyright is the original author's.
