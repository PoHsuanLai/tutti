#![doc = include_str!("../README.md")]
//! Polyphonic subtractive synthesis for the Tutti audio engine.
//!
//! Use it when you want a playable synthesizer voice in a Tutti graph: saw,
//! square, triangle, sine or noise oscillators through a Moog-style ladder or a
//! state-variable filter, with an ADSR envelope, unison, portamento, alternative
//! tunings and MPE / MIDI 2.0 per-note expression.
//!
//! `PolySynth` is the synth: a graph node (`tutti_graph::Node`) built, infallibly,
//! from a `SynthConfig`, with no audio input, stereo output and one MIDI event input.
//! Wire a clip node, a keyboard's event inlet (`tutti_graph::EventInletNode`) or the hardware input node (all
//! `tutti-midi-runtime`) to the event input; each event is applied on its own
//! frame. The rest of the crate is configuration: `SynthConfig` with its
//! `OscillatorType`, `FilterType`, `Adsr` and `FilterModConfig`; voice
//! allocation (`VoiceMode`, `AllocationStrategy`); `UnisonConfig`;
//! `PortamentoConfig`; and `Tuning`.
//!
//! Inserted into a graph, the synth hands back a `tutti_graph::ParamSet` over its
//! live params by `UnitParam`: the master `Volume` and, with a unison engine,
//! `Detune` and `StereoSpread`. That set is the synth's only control once it is in
//! a graph (a modulation lane's knob is its cell; there are no cell handles on the
//! synth). A fork of the graph (an offline export) starts from the values last set
//! through it. The rate is the graph's too: `SynthConfig` carries none, and
//! `prepare` sets it.
//!
//! ## Quick start
//!
//! ```rust
//! use std::num::NonZeroUsize;
//! use tutti_types::MaxBlock;
//! use tutti_polysynth::{
//!     Adsr, FilterType, OscillatorType, PolySynth, SynthConfig,
//! };
//! use tutti_graph::{OutPort, Source};
//! use tutti_types::{Amplitude, Hz, Resonance, SampleRate, Samples, Seconds, UnitParam};
//! use tutti_graph::NodeKey;
//! use tutti_graph::{Editor, Prepare};
//!
//! // `Moog` takes `Resonance`; the `Svf` variant takes `Q` instead.
//! let synth = PolySynth::new(SynthConfig {
//!     oscillator: OscillatorType::Saw,
//!     polyphony: NonZeroUsize::new(8).unwrap(),
//!     filter: FilterType::Moog {
//!         cutoff: Hz(2000.0),
//!         resonance: Resonance(0.7),
//!     },
//!     envelope: Adsr {
//!         attack: Seconds(0.01),
//!         decay: Seconds(0.2),
//!         sustain: Amplitude(0.6),
//!         release: Seconds(0.3),
//!     },
//!     ..Default::default()
//! });
//!
//! // Into the graph: no audio input, stereo out, one MIDI event input (wire a
//! // clip or a keyboard's queue node to it with `GraphSpec::connect_events`).
//! let (mut editor, _executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(512))));
//! let synth_key = NodeKey(1);
//! let params = editor.insert(synth_key, "synth", synth);
//! editor.spec_mut().topology.set_outputs((0..2)
//!     .map(|port| Source::Node(OutPort { node: synth_key, port })));
//! editor.commit().expect("a one-node graph commits");
//!
//! // Its controls: the live params, by address.
//! assert!(params.set(UnitParam::Volume, 0.8));
//! ```
//!
//! ## What is fixed at construction
//!
//! The voice bank every voice renders in is built once for the oscillator, filter
//! type and envelope, so those need a new synth to change: build a new
//! `PolySynth` and replace the node. The filter's cutoff (and a ladder's
//! resonance) are modulated live by `FilterModConfig`, CC74, CC71 and MPE slide.
//!
//! Nothing else on a synth that is in a graph is live but its `ParamSet`: the
//! master volume, and with a unison engine the detune and stereo spread. Before
//! insertion, `&mut self` setters take typed values (`set_volume(Amplitude)`),
//! and a node cannot be reached by them once it is inserted.
//!
//! `SynthConfig::polyphony` is a `NonZeroUsize` (a synth of no voices cannot be
//! written) with no upper bound. Every voice is built up front, so it is a memory
//! and CPU budget. A unison stack is `UnisonVoices`, 1 to
//! 16. MPE zones are not the synth's: classic MPE is rewritten into per-note messages
//! before it arrives, and the per-note bend range is 48 semitones until a channel's
//! RPN 0/7 says otherwise.
//!
//! ## Real-time behaviour
//!
//! Rendering allocates nothing and takes no lock, at any block length. Up to 512
//! MIDI events per block are applied; any beyond that are dropped. Building a
//! synth, forking it and changing the unison voice count allocate, and belong on
//! the control thread.
//!
//! ## Scope
//!
//! - **Not a SoundFont player.** `.sf2` playback is `tutti-soundfont`, a separate
//!   crate that shares only the graph-node shape with this one.
//! - **Not a clip player.** Playing recorded audio on a timeline is
//!   `tutti-sampler`.
//! - **No effects.** Filters live per voice inside the synth; a send, a delay or
//!   a reverb is a `tutti-nodes` node after it.
//! - **No release velocity.** A note-off starts the envelope's release whatever
//!   its velocity.
//! - **No MIDI I/O and no file parsing.** The message types are
//!   `tutti-ump`'s, the MIDI nodes are `tutti-midi-runtime`, device ports
//!   are `tutti-midi-hardware`.
//!
//! The `tutti` crate re-exports this one as `tutti::polysynth` behind its
//! `synth` feature.
//!
//! ## Verification
//!
//! `testing/tutti-verify` renders a set of configurations to disk and checks
//! the output against synthesis theory (`cargo xtask verify-audio`).
//!
//! ## Features
//!
//! `default = []`. The `std` feature is accepted but currently gates nothing.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - [`PolySynth`]: the synth, a graph node with one MIDI event input.
//! - [`SynthConfig`]: everything a synth is built from, with
//!   [`OscillatorType`], [`FilterType`] and [`SvfMode`], [`Adsr`]
//!   and [`FilterModConfig`].
//! - Voice allocation: [`VoiceMode`] and [`AllocationStrategy`].
//! - [`UnisonConfig`] (with [`UnisonVoices`]), [`PortamentoConfig`] (with [`PortamentoMode`] and
//!   the `tutti_curve::Law` of its `curve`) and [`Tuning`].

mod note_id;
mod voice;
pub(crate) use voice::{VoiceAllocator, VoiceAllocatorConfig};

mod mpe;
pub(crate) use mpe::MpeVoiceState;
// Public: these appear in `SynthConfig`'s fields.
pub use voice::{AllocationStrategy, VoiceMode};

mod unison;
pub(crate) use unison::UnisonEngine;
pub(crate) use unison::UnisonVoiceParams;
// Public: `UnisonConfig` appears in `SynthConfig`, and its count is `UnisonVoices`.
pub use unison::{UnisonConfig, UnisonVoices, UnisonVoicesError};

mod portamento;
pub(crate) use portamento::Portamento;
// Public: these appear in `SynthConfig` (`portamento` field + its config).
pub use portamento::{GlideTime, GlideTimeError, PortamentoConfig, PortamentoMode};

mod tuning;
// Public: `Tuning` appears in `SynthConfig`.
pub use tuning::Tuning;

mod synth;
pub use synth::{FilterModConfig, FilterType, OscillatorType, SvfMode, SynthConfig};
// The amplitude envelope `SynthConfig::envelope` takes; one type with the
// modulation sources' note envelope.
pub use tutti_types::Adsr;

mod polysynth;
// `PolySynth::fork_instance`: the synth in a fork of the
// graph (an export), with its clip.
mod fork;
pub use polysynth::PolySynth;

mod synth_voice;

mod bank;
mod kernel;
