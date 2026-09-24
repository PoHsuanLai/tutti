# tutti-sampler

Sample playback and disk streaming for the Tutti audio engine, plus the
phase-vocoder time stretch.

## What this is

Voices that play a `Wave`, and the butler thread that keeps a streamed one fed
from disk. `Wave` and decoding are [`tutti-io`](../../core/tutti-io)'s (the
decoder is [symphonia](https://crates.io/crates/symphonia)); this crate's codec
features forward to that crate's.

There is no `Sampler` façade type and no channel-strip builder. The entry points
are the voices themselves — `MemorySource` and `DiskVoice` — plus `VoicePool`
over them and `DiskStreamer`, the handle that owns the streaming engine.

## What it does not own

- **Devices.** The CPAL stream and the RT callback are `tutti-cpal`'s.
- **Recording, and any live WAV sink.** `WavOut` and `Recorder` are
  [`tutti-io`](../../core/tutti-io)'s, so the device layer reaches them without
  depending on a DSP crate.
- **SoundFont playback.** That is [`tutti-soundfont`](../tutti-soundfont), a
  separate crate — not this one, and not a feature of
  [`tutti-polysynth`](../tutti-polysynth) either. A `.sf2` player shares no voice
  engine with a clip player or a subtractive synth.
- **Note-driven playback.** This crate is a clip/timeline player: it has no
  `note_on`, no keymap and no voice stealing. A note is a synth's noun; see
  `tutti-polysynth` or `tutti-soundfont`.

## Two playback tiers, one vocabulary

A voice plays either from memory (`MemorySource`) or streamed from disk
(`DiskVoice`, fed by the butler thread). **The tier is the caller's choice** —
the sampler never picks one on its own — and `VoicePool` mixes both behind one
command surface. Where a verb only makes sense on one tier, the `VoiceSource`
match says so at the call site instead of silently no-opping.

Rates are typed to keep the tiers honest: `PlaybackRate` is varispeed (couples
pitch), `SrcRatio` is sample-rate conversion (derived, never user intent), and
`StretchFactor` drives the phase vocoder (pitch-independent). They compose only
through `PlaybackRate::read_rate`, and the varispeed range lives in one shared
bounded constructor that every user-input path goes through — a bound that sits
in one tier's setter is a bound the other tier silently ignores.

A voice bound to a transport derives its read position from the playhead every
frame rather than carrying a cursor, matching `tutti-core`'s transport: one clock
advances, everything else reads.

## Quick start

An in-memory voice needs no butler and no file, so it is a plain graph node:
build the `Wave`, wrap it, push it into a `Net` and render.

```rust
use std::sync::Arc;
use tutti_core::dsp::Net;
use tutti_core::AudioUnit;
use tutti_io::Wave;
use tutti_sampler::MemorySource;

// 100 stereo FRAMES — `push_frame` takes one frame, not one sample.
let mut wave = Wave::new(2, 44_100.0);
for _ in 0..100 {
    wave.push_frame(&[0.5, 0.5]);
}

let source = MemorySource::new(Arc::new(wave));
source.play();

// No audio input: the voice *is* the source. Stereo out.
let mut net = Net::new(0, 2);
let voice = net.push(Box::new(source));
net.pipe_output(voice);
net.check();

let mut out = [0.0f32; 2];
net.tick(&[], &mut out);
```

Streaming is the other tier and cannot run here — it needs a real file on disk.
Build a `DiskStreamer` once with `DiskStreamer::new`, then drive it through the
`commands()` WRITE port and the `status()` READ port; `status()` is also what
constructs the `DiskVoice` to wire into the graph. `DiskStreamer`'s own rustdoc
carries that example.

## Bevy-free use

The engine is Bevy-free; the `bevy` feature adds `derive(Component)` on the
voice-pool handles and nothing else — no plugin and no asset loader, both of
which are `bevy-tutti`'s. Every DSP leaf above compiles and runs without it.

## Constraint: the read width has a ceiling, and it is not arbitrary

`MAX_SAMPLER_CHANNELS` equals `tutti_core::MAX_ROOT_CHANNELS`, and **the two move
together**: a voice wider than the graph root can render is a voice nobody can
hear, and letting the sampler exceed it would mean the truncation happened
silently downstream at the root's fold rather than visibly here. The engine's
other ceilings are *not* interchangeable with it — export folds at 12
(`MAX_NET_CHANNELS`) because an offline render is not bound by the live stack
scratch, and the plugin hosts use 16 because a plugin's bus width is its own
business.

## Features

`default = ["wav"]`.

- `wav` / `flac` / `mp3` / `ogg` — decoder support, each cascading to the
  matching `tutti-core` feature. `files` turns on all four.
- `bevy` — the entity-as-node marker derives on the voice-pool handles.

With no format feature at all there is no header to read, so `probe` is **absent**
rather than always answering "not streamable": a host that compiled out every
codec cannot open files, and a silent `false` would look like a property of the
file.

## License

MIT OR Apache-2.0
