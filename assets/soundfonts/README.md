# SoundFont Assets

RustySynth is a SoundFont **player** - you need to provide a .sf2 file.

`TimGM6mb.sf2` is **committed** here: it is the fixture for the SoundFont test
suites (`tutti-soundfont`'s `tests/`, and `bevy-tutti`'s `midi_soundfont` under
its `midi` + `soundfont` features). Those resolve it from their own
`CARGO_MANIFEST_DIR` up to the repo root and fail loudly, naming the path, when
it is missing — never skip.

`download-timgm6mb.sh` re-fetches it into the current directory, for a checkout
that lost it:

```bash
cd assets/soundfonts
./download-timgm6mb.sh
```

## The file

- **TimGM6mb.sf2** (5.7 MB)
- Source: Debian package archive
- License: GNU GPL
- General MIDI compatible soundfont
