# tutti-codec

**The audio file encoders**: WAV, AIFF, FLAC and Ogg Vorbis behind one
`Encoder`, over any `Write + Seek` sink.

## What is here

- `Format` names the container and carries only the settings it accepts:
  `Wav(Pcm)`, `Aiff(Pcm)`, `Flac { depth: IntDepth, level: FlacLevel }` and
  `Vorbis { quality: Quality }`. FLAC at float and Ogg at a sample width are not
  expressible. Each variant exists only with its feature (`wav`, `aiff`, `flac`,
  `vorbis`), none on by default.
- `Spec` is what a file is written as: the format, the width, the rate and the
  speaker order.
- `Encoder` is `create(spec, sink)`, a fallible `write(block)` and `finish()`.
  `Encoder::create_file` writes beside the path and moves the file into place
  on `finish`; dropping the encoder first removes it.
- `Pcm`, `IntDepth` and `Sample` are the sample widths and the one quantization
  every container shares.

## What this crate does not own

The pipeline in front of the encoder: pulling blocks from a render or a live
source, resampling and dithering. An encoder writes the blocks it is given at
the rate and width the `Spec` states. `tutti-export` is the offline pipeline and
`tutti-io`'s `WavOut` the live one; both write through this crate, so a bounced
and a recorded file quantize and lay out a given sample identically.
