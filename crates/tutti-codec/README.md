# tutti-codec

**The audio file codecs**: WAV, AIFF, FLAC and Ogg Vorbis out behind one
`Encoder`, over any `Write + Seek` sink; WAV, FLAC, MP3 and Ogg in as a `Wave`
or a streaming `FileIn`.

## What is here

- `Format` names the container and carries only the settings it accepts:
  `Wav(Pcm)`, `Aiff(Pcm)`, `Flac { depth: IntDepth, level: FlacLevel }` and
  `Vorbis { quality: Quality }`. FLAC at float and Ogg at a sample width are not
  expressible. Each variant exists only with its feature (`encode-wav`, `encode-aiff`, `encode-flac`,
  `encode-vorbis`), none on by default.
- `Spec` is what a file is written as: the format, the width, the rate and the
  speaker order.
- `Encoder` is `create(spec, sink)`, a fallible `write(block)` and `finish()`.
  `Encoder::create_file` writes beside the path and moves the file into place
  on `finish`; dropping the encoder first removes it, and so does `finish` after a
  failed `write`.
- Decoding is plain functions and types of our own, no trait: `Wave::read`
  decodes a file into a `Wave` (a resident, planar sample buffer),
  `Wave::read_metadata` reads a header, `Wave::from_bytes` decodes a file held
  in memory, and `FileIn` streams a file as an `AudioIn` with a
  sample-accurate `seek`. A failure is a `DecodeError`. `symphonia` does the
  work behind them, in `src/decode/` alone, and none of its types is in a
  signature or an error. `can_decode` and `decodable_extensions` say what this
  build reads. Each `decode-*` feature (`decode-wav`, `decode-flac`,
  `decode-mp3`, `decode-ogg`) pulls exactly one container.
- `Pcm`, `IntDepth` and `Sample` are the sample widths and the one quantization
  every container shares.

## What this crate does not own

The pipeline in front of the encoder: pulling blocks from a render or a live
source, resampling and dithering. An encoder takes blocks already at the rate
and width the `Spec` states, and nothing here stops a caller from handing it
others. `tutti-export` is the offline pipeline and
`tutti-io`'s `WavOut` the live one; both write through this crate, so a bounced
and a recorded file quantize and lay out a given sample identically.
