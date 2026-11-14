# Introduction

tutti is a real-time audio engine for building DAWs in Rust: the part of an
audio program that runs underneath the interface, keeps the sound going and
keeps it in time.

Audio software has always felt a little chaotic to us: formats with their own
rules, specs with their own corners, vendor quirks no document mentions, and
systems code and DSP math in the same program. tutti is our attempt to tame
that chaos with Rust's expressive type system. A frequency is a `Hz`, a gain
is a `Db`, a length is a count of frames, and a value that would be wrong
does not compile.

## What you can make with it

- A music tool: a sequencer, a sampler, a groove box, a DAW of your own.
- The audio of a game or an installation, where sound follows what happens on
  screen.
- An offline renderer: a program that writes a mix, a set of stems or a
  measured master to disk with no sound card at all.

In this book we build sound step by step. Each program is short, each one
compiles, and each one that makes a sound lets you hear it right on the page.

## What tutti is not

tutti has no user interface of its own: it draws no windows, knobs or
timelines. Your program draws those and asks tutti for the sound. It is not
a finished DAW either; it is what one is built on.

## How the book is laid out

1. **Getting started** sets up a project and makes a first sound.
2. **The guide** builds one piece of music from nothing, a chapter at a time:
   a sound, then shaping it, movement, time, notes, recordings, mixing, and
   finally bouncing it to files.
3. **Building blocks** goes back over each idea the guide used, one chapter
   each, from the smallest piece up.
4. **Going further** covers live audio and MIDI devices, hosting plugins,
   writing your own node, and analysis without a graph.

You need to know Rust. You do not need to be an audio engineer: when the book
uses an audio idea, a short note explains it, and you can fold it away when
you already know it.

The book teaches; the API reference lists everything. Build it with
`cargo doc -p tutti --open`.
