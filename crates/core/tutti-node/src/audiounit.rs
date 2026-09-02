//! The `AudioUnit` trait: the dynamic node interface the whole engine is built
//! on, and the one thing every processor in the graph implements.
//!
//! The trait is object-safe by construction — `Net` stores `Box<dyn AudioUnit>`
//! and is itself an `AudioUnit` — and generic over the sample format `S`, with
//! `F32` as the default so the overwhelmingly common `Box<dyn AudioUnit>` reads
//! unchanged. The generic is load-bearing rather than speculative: the plugin
//! hosts implement `AudioUnit<F64>`, so it cannot be specialized away.
//!
//! # What is here and what is not
//!
//! This module holds the trait and nothing else. The implementations that used
//! to sit beside it — `An<X>` (the `AudioNode` bridge), `Unit`,
//! `BigBlockAdapter`, `BlockRateAdapter`, `DummyUnit` — stayed in
//! `fundsp-tutti`, because each names `AudioNode` or `An`, which are that
//! crate's static-arity vocabulary and not part of the dynamic contract.
//!
//! Reading the split the other way round: what is here is exactly what a crate
//! outside the fork needs in order to *be* a node, and the 43 `impl AudioUnit`
//! sites across the engine confirm the line — every one of them implements this
//! trait and none of them defines an `AudioNode`.

use crate::buffer::{BufferMut, BufferRef};
use crate::math::{abs, amp_db, ceil, floor, max, round, AttoHash};
use crate::num::{Num, Sample, F32};
use crate::setting::Setting;
use crate::signal::{Signal, SignalFrame};
use crate::value::Tail;
use dyn_clone::DynClone;
use num_complex::Complex64;

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

/// An audio processor with an object safe interface.
/// Once constructed, it has a fixed number of inputs and outputs.
///
/// Generic over sample format `S`. The default `S = F32` means existing code
/// using `AudioUnit` or `Box<dyn AudioUnit>` continues to work unchanged as f32.
/// Use `AudioUnit<F64>` for native f64 processing.
pub trait AudioUnit<S: Sample = F32>: Send + Sync + DynClone {
    /// Reset the input state of the unit to an initial state where it has not processed any data.
    /// In other words, reset time to zero.
    fn reset(&mut self) {
        // The default implementation does nothing.
    }

    /// Sever any shared *live* I/O so this unit is safe to tick in isolation on
    /// a worker thread, concurrently with the live graph.
    ///
    /// `Net::clone` (run on every `commit()`) shares some units' live input
    /// handles by `Arc` — e.g. a synth's MIDI inbox, a clip reader's command
    /// channel. That is correct for the frontend↔backend swap, where only one
    /// instance is ever ticked. But an *offline* clone (a region render) is
    /// ticked on a worker thread **while the original keeps playing**; a shared
    /// inbox means the worker drains events/commands the live unit needs (each
    /// is delivered to exactly one consumer), garbling live playback.
    ///
    /// Implementors that hold shared live input reset it to fresh, dead, empty
    /// local state here — after `isolate()` the unit reads nothing from the
    /// live world and steals nothing from it. Pure-DSP units share no live I/O,
    /// so the default does nothing. Called by the offline-render isolation pass
    /// on every node of the cloned net before it reaches the worker.
    ///
    /// This severs *inputs only*; re-pointing a unit at offline data (transport,
    /// scheduled events) is the data-carrying step [`Self::rebind_offline`]
    /// performs immediately after.
    fn isolate(&mut self) {
        // The default implementation does nothing.
    }

    /// Re-point this unit at the offline render's data — the half `isolate()`
    /// cannot do, because it carries no data.
    ///
    /// `isolate()` leaves a severed unit still aiming at the *live* transport,
    /// which nothing advances offline: a voice bound to it reads a frozen
    /// playhead and renders silence. Implementors holding a transport (or their
    /// own clock) re-seat it here.
    ///
    /// # Why the context is `&dyn Any`
    ///
    /// The offline context names a timeline, and this crate cannot name one —
    /// `Timeline` lives in `tutti-core`, which depends on *this* crate. Passing
    /// it opaquely keeps the hook where every node already is (beside `isolate`,
    /// reached through `Net` without a type switch) while letting the transport
    /// vocabulary stay downstream. Implementors downcast it once:
    ///
    /// ```ignore
    /// fn rebind_offline(&mut self, ctx: &dyn Any) {
    ///     let Some(transport) = ctx.downcast_ref::<OfflineTransport>() else { return };
    ///     self.transport = transport.clone();
    /// }
    /// ```
    ///
    /// **A transport-aware unit that does not implement this renders against
    /// the live playhead**, and does so silently — there is no value to compare
    /// and no error to raise. That is exactly why this is a defaulted method on
    /// the node rather than a match arm in the renderer: a new node declares its
    /// own rebinding and is covered automatically, where a renderer-side ladder
    /// would skip anything it had not been taught to name. Pure-DSP units are
    /// unaffected by time-of-render and correctly do nothing.
    #[allow(unused_variables)]
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        // The default implementation does nothing.
    }

    /// Set the sample rate of the unit.
    /// The default sample rate is 44100 Hz.
    /// The unit is allowed to reset itself here in response to sample rate changes.
    /// If the sample rate stays unchanged, then the goal is to maintain current state.
    #[allow(unused_variables)]
    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        // The default implementation does nothing.
    }

    /// Process one sample.
    /// The length of `input` and `output` must be equal to `inputs` and `outputs`, respectively.
    fn tick(&mut self, input: &[S::Scalar], output: &mut [S::Scalar]);

    /// Process up to 64 (MAX_BUFFER_SIZE) samples.
    /// If `size` is zero then this is a no-op, which is permitted.
    fn process(&mut self, size: usize, input: &BufferRef<S>, output: &mut BufferMut<S>);

    /// Set a parameter. What formats are recognized depends on the component.
    #[allow(unused_variables)]
    fn set(&mut self, setting: Setting) {}

    /// Number of inputs to this unit.
    /// Equals size of the input argument in `tick` and `process`.
    /// This should be fixed after construction.
    fn inputs(&self) -> usize;

    /// Number of outputs from this unit.
    /// Equals size of the output argument in `tick` and `process`.
    /// This should be fixed after construction.
    fn outputs(&self) -> usize;

    /// Route constants, latencies and frequency responses at `frequency` Hz
    /// from inputs to outputs. Return output signal.
    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame;

    /// Return an ID code for this type of unit.
    fn get_id(&self) -> u64;

    /// Downcast this AudioUnit to `&dyn Any` for type inspection.
    /// This enables safe runtime type checking and downcasting.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Downcast this AudioUnit to `&mut dyn Any` for type inspection and mutation.
    /// This enables safe runtime type checking and downcasting.
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any;

    /// Set unit pseudorandom phase hash. Override this to use the hash.
    /// This is called from `ping` (only). It should not be called by users.
    #[allow(unused_variables)]
    fn set_hash(&mut self, hash: u64) {
        // The default implementation does nothing.
    }

    /// Ping contained `AudioUnit`s and `AudioNode`s to obtain
    /// a deterministic pseudorandom hash. The local hash includes children, too.
    /// Leaf nodes should not need to override this.
    /// If `probe` is true, then this is a probe for computing the network hash
    /// and `set_hash` should not be called yet.
    /// To set a custom hash for a graph, call this method with `ping`
    /// set to false and `hash` initialized with the custom hash.
    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        if !probe {
            self.set_hash(hash.state());
        }
        hash.hash(self.get_id())
    }

    /// Memory footprint of this unit in bytes, without counting buffers and other allocations.
    fn footprint(&self) -> usize;

    /// Preallocate all needed memory, including buffers for block processing.
    fn allocate(&mut self) {
        // The default implementation does nothing.
    }

    // End of interface. There is no need to override the following.

    /// Retrieve the next mono sample from a generator.
    /// The node must have no inputs and 1 or 2 outputs.
    /// If there are two outputs, average them.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(dc(2.0).get_mono(), 2.0);
    /// assert_eq!(dc((3.0, 4.0)).get_mono(), 3.5);
    /// ```
    #[inline]
    fn get_mono(&mut self) -> S::Scalar {
        debug_assert!(self.inputs() == 0);
        match self.outputs() {
            1 => {
                let mut output = [S::scalar_zero()];
                self.tick(&[], &mut output);
                output[0]
            }
            2 => {
                let mut output = [S::scalar_zero(); 2];
                self.tick(&[], &mut output);
                (output[0] + output[1]) * S::Scalar::from_f64(0.5)
            }
            _ => panic!("AudioUnit::get_mono(): Unit must have 1 or 2 outputs"),
        }
    }

    /// Retrieve the next stereo sample (left, right) from a generator.
    /// The node must have no inputs and 1 or 2 outputs.
    /// If there is just one output, duplicate it.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(dc((5.0, 6.0)).get_stereo(), (5.0, 6.0));
    /// assert_eq!(dc(7.0).get_stereo(), (7.0, 7.0));
    /// ```
    #[inline]
    fn get_stereo(&mut self) -> (S::Scalar, S::Scalar) {
        debug_assert!(self.inputs() == 0);
        match self.outputs() {
            1 => {
                let mut output = [S::scalar_zero()];
                self.tick(&[], &mut output);
                (output[0], output[0])
            }
            2 => {
                let mut output = [S::scalar_zero(); 2];
                self.tick(&[], &mut output);
                (output[0], output[1])
            }
            _ => panic!("AudioUnit::get_stereo(): Unit must have 1 or 2 outputs"),
        }
    }

    /// Filter the next mono sample `x`.
    /// The node must have exactly 1 input and 1 output.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(add(4.0).filter_mono(5.0), 9.0);
    /// ```
    #[inline]
    fn filter_mono(&mut self, x: S::Scalar) -> S::Scalar {
        debug_assert!(self.inputs() == 1 && self.outputs() == 1);
        let mut output = [S::scalar_zero()];
        self.tick(&[x], &mut output);
        output[0]
    }

    /// Filter the next stereo sample `(x, y)`.
    /// The node must have exactly 2 inputs and 2 outputs.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(add((2.0, 3.0)).filter_stereo(4.0, 5.0), (6.0, 8.0));
    /// ```
    #[inline]
    fn filter_stereo(&mut self, x: S::Scalar, y: S::Scalar) -> (S::Scalar, S::Scalar) {
        debug_assert!(self.inputs() == 2 && self.outputs() == 2);
        let mut output = [S::scalar_zero(); 2];
        self.tick(&[x, y], &mut output);
        (output[0], output[1])
    }

    /// Evaluate frequency response of `output` at `frequency` Hz.
    /// Any linear response can be composed.
    /// Return `None` if there is no response or it could not be calculated.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(pass().response(0, 440.0), Some(Complex64::new(1.0, 0.0)));
    /// ```
    fn response(&mut self, output: usize, frequency: f64) -> Option<Complex64> {
        assert!(output < self.outputs());
        let mut input = SignalFrame::new(self.inputs());
        for i in 0..self.inputs() {
            input.set(i, Signal::Response(Complex64::new(1.0, 0.0), 0.0));
        }
        let response = self.route(&input, frequency);
        match response.at(output) {
            Signal::Response(rx, _) => Some(rx),
            _ => None,
        }
    }

    /// Evaluate frequency response of `output` in dB at `frequency` Hz.
    /// Any linear response can be composed.
    /// Return `None` if there is no response or it could not be calculated.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// let db = pass().response_db(0, 440.0).unwrap();
    /// assert!(db < 1.0e-7 && db > -1.0e-7);
    /// ```
    fn response_db(&mut self, output: usize, frequency: f64) -> Option<f64> {
        assert!(output < self.outputs());
        self.response(output, frequency).map(|r| amp_db(r.norm()))
    }

    /// Causal latency in (fractional) samples.
    /// After a reset, we can discard this many samples from the output to avoid incurring a pre-delay.
    /// The latency may depend on the sample rate.
    ///
    /// ### Example
    /// ```
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(pass().latency(), Some(0.0));
    /// assert_eq!(tick().latency(), Some(0.0));
    /// assert_eq!(sink().latency(), None);
    /// assert_eq!(limiter(0.01, 0.01).latency(), Some(441.0));
    /// ```
    fn latency(&mut self) -> Option<f64> {
        if self.outputs() == 0 {
            return None;
        }
        let mut input = SignalFrame::new(self.inputs());
        for i in 0..self.inputs() {
            input.set(i, Signal::Latency(0.0));
        }
        // The frequency argument can be anything as there are no responses to propagate,
        // only latencies. Latencies are never promoted to responses during signal routing.
        let response = self.route(&input, 1.0);
        // Return the minimum latency.
        let mut result: Option<f64> = None;
        for output in 0..self.outputs() {
            match (result, response.at(output)) {
                (None, Signal::Latency(x)) => result = Some(x),
                (Some(r), Signal::Latency(x)) => result = Some(r.min(x)),
                _ => (),
            }
        }
        result
    }

    /// How long this unit keeps producing after its input stops.
    ///
    /// Unlike [`latency`](Self::latency), this cannot be derived from
    /// [`route`](Self::route): a [`Signal`] carries a latency through a chain,
    /// and there is no equivalent carrier for a decay. A unit that has one
    /// therefore reports it here, and [`tutti_types::tail`] composes the graph's
    /// figure from what each node says.
    ///
    /// The default is [`Tail::Unknown`] — a unit that has not been taught to
    /// answer has said nothing, which is not the same as saying it has no tail.
    /// Overriding with [`Tail::None`] is how a unit states that it stops with
    /// its input.
    fn tail(&mut self) -> Tail {
        Tail::Unknown
    }

    /// Print information about this unit into a string.
    fn display(&mut self) -> String {
        let mut string = String::new();

        if self.inputs() > 0 && self.outputs() > 0 && self.response(0, 440.0).is_some() {
            let scope = [
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
            ];

            let mut scope: Vec<_> = scope.iter().map(|x| x.to_vec()).collect();

            let f: [f64; 48] = [
                10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 120.0, 140.0, 160.0,
                180.0, 200.0, 250.0, 300.0, 350.0, 400.0, 450.0, 500.0, 600.0, 700.0, 800.0, 900.0,
                1000.0, 1200.0, 1400.0, 1600.0, 1800.0, 2000.0, 2500.0, 3000.0, 3500.0, 4000.0,
                4500.0, 5000.0, 6000.0, 7000.0, 8000.0, 9000.0, 10000.0, 12000.0, 14000.0, 16000.0,
                18000.0, 20000.0, 22000.0,
            ];

            let r: Vec<_> = f
                .iter()
                .map(|&f| (self.response_db(0, f).unwrap(), f))
                .collect();

            let epsilon_db = 1.0e-2;
            let max_r = r.iter().fold((-f64::INFINITY, None), {
                |acc, &x| {
                    if abs(acc.0 - x.0) <= epsilon_db {
                        (max(acc.0, x.0), None)
                    } else if acc.0 > x.0 {
                        acc
                    } else {
                        (x.0, Some(x.1))
                    }
                }
            });
            let max_db = ceil(max_r.0 / 10.0) * 10.0;

            for i in 0..f.len() {
                let row = (max_db - r[i].0) / 5.0;
                let mut j = ceil(row) as usize;
                let mut c = if row - floor(row) <= 0.5 { b'*' } else { b'.' };
                while j < scope.len() {
                    scope[j][i] = c;
                    j += 1;
                    c = b'*';
                }
            }

            for (row, ascii_line) in scope.into_iter().enumerate() {
                let line = String::from_utf8(ascii_line).unwrap();
                if row & 1 == 0 {
                    let db = round(max_db - row as f64 * 5.0) as i64;
                    writeln!(&mut string, "{:3} dB {} {:3} dB", db, line, db).unwrap();
                } else {
                    writeln!(&mut string, "       {}", line).unwrap();
                }
            }

            writeln!(
                &mut string,
                "       |   |    |    |     |    |    |     |    |    |"
            )
            .unwrap();
            writeln!(
                &mut string,
                "       10  50   100  200   500  1k   2k    5k   10k  20k Hz\n"
            )
            .unwrap();

            write!(&mut string, "Peak Magnitude : {:.2} dB", max_r.0).unwrap();

            match max_r.1 {
                Some(frequency) => {
                    writeln!(&mut string, " ({} Hz)", frequency as i64).unwrap();
                }
                _ => {
                    string.push('\n');
                }
            }
        }

        writeln!(&mut string, "Inputs         : {}", self.inputs()).unwrap();
        writeln!(&mut string, "Outputs        : {}", self.outputs()).unwrap();
        writeln!(
            &mut string,
            "Latency        : {:.1} samples",
            self.latency().unwrap_or(0.0)
        )
        .unwrap();
        writeln!(&mut string, "Footprint      : {} bytes", self.footprint()).unwrap();

        string
    }
}

dyn_clone::clone_trait_object!(<S> AudioUnit<S> where S: Sample);
