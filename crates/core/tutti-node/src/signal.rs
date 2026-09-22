//! Signal flow analysis components.

use crate::math::min;
use num_complex::Complex64;
extern crate alloc;
use tinyvec::TinyVec;

/// Contents of a mono signal. Used in latency and frequency response analysis.
#[derive(Clone, Copy, Default)]
pub enum Signal {
    /// Signal with unknown properties.
    #[default]
    Unknown,
    /// Constant signal with value.
    Value(f64),
    /// Signal that is connected to inputs or generators with latency in samples.
    Latency(f64),
    /// Signal that is connected to inputs or generators
    /// with complex frequency response and latency in samples.
    Response(Complex64, f64),
}

impl Signal {
    /// Filter signal using the frequency response function `filter`
    /// while adding extra `latency`. Latency is measured in samples.
    pub fn filter(&self, latency: f64, filter: impl Fn(Complex64) -> Complex64) -> Signal {
        match self {
            Signal::Latency(l) => Signal::Latency(l + latency),
            Signal::Response(response, l) => Signal::Response(filter(*response), l + latency),
            _ => Signal::Unknown,
        }
    }

    /// Apply nonlinear processing to signal with extra `latency` in samples.
    /// Nonlinear processing erases constant values and frequency responses but maintains latency.
    /// Latency is measured in samples.
    pub fn distort(&self, latency: f64) -> Signal {
        match self {
            Signal::Latency(l) => Signal::Latency(l + latency),
            Signal::Response(_, l) => Signal::Latency(l + latency),
            _ => Signal::Unknown,
        }
    }

    /// Delay signal by `latency` samples.
    pub fn delay(&self, latency: f64) -> Signal {
        match self {
            Signal::Latency(l) => Signal::Latency(l + latency),
            Signal::Response(response, l) => Signal::Response(*response, l + latency),
            x => *x,
        }
    }

    /// Scale signal by `factor`.
    pub fn scale(&self, factor: f64) -> Signal {
        match self {
            Signal::Value(x) => Signal::Value(x * factor),
            Signal::Response(response, latency) => Signal::Response(response * factor, *latency),
            x => *x,
        }
    }

    /// Combine signals nonlinearly with extra `latency`.
    /// Nonlinear processing erases constant values and frequency responses but maintains latency.
    /// Latency is measured in samples.
    pub fn combine_nonlinear(&self, other: Signal, latency: f64) -> Signal {
        match (self.distort(0.0), other.distort(0.0)) {
            (Signal::Latency(lx), Signal::Latency(ly)) => Signal::Latency(min(lx, ly) + latency),
            (Signal::Latency(lx), _) => Signal::Latency(lx + latency),
            (_, Signal::Latency(ly)) => Signal::Latency(ly + latency),
            _ => Signal::Unknown,
        }
    }

    /// Combine signals linearly with extra `latency` and `value` and `response` processing as functions.
    /// Constant signals are considered as zero responses.
    /// Latency is measured in samples.
    pub fn combine_linear(
        &self,
        other: Signal,
        latency: f64,
        value: impl Fn(f64, f64) -> f64,
        response: impl Fn(Complex64, Complex64) -> Complex64,
    ) -> Signal {
        match (*self, other) {
            (Signal::Value(vx), Signal::Value(vy)) => Signal::Value(value(vx, vy)),
            (Signal::Latency(lx), Signal::Latency(ly)) => Signal::Latency(min(lx, ly) + latency),
            (Signal::Response(rx, lx), Signal::Response(ry, ly)) => {
                Signal::Response(response(rx, ry), min(lx, ly) + latency)
            }
            (Signal::Response(rx, lx), Signal::Value(_)) => {
                Signal::Response(response(rx, Complex64::new(0.0, 0.0)), lx + latency)
            }
            (Signal::Value(_), Signal::Response(ry, ly)) => {
                Signal::Response(response(Complex64::new(0.0, 0.0), ry), ly + latency)
            }
            (Signal::Response(_, lx), Signal::Latency(ly)) => {
                Signal::Latency(min(lx, ly) + latency)
            }
            (Signal::Latency(lx), Signal::Response(_, ly)) => {
                Signal::Latency(min(lx, ly) + latency)
            }
            (Signal::Latency(lx), _) => Signal::Latency(lx + latency),
            (Signal::Response(_, lx), _) => Signal::Latency(lx + latency),
            (_, Signal::Latency(ly)) => Signal::Latency(ly + latency),
            (_, Signal::Response(_, ly)) => Signal::Latency(ly + latency),
            _ => Signal::Unknown,
        }
    }
}

/// Frame of input or output signals. Up to 16 channels can be analyzed on stack.
#[derive(Clone)]
pub struct SignalFrame(TinyVec<[Signal; 16]>);

impl SignalFrame {
    /// Create a new signal frame with all channels marked unknown.
    pub fn new(channels: usize) -> SignalFrame {
        let mut frame = SignalFrame(TinyVec::with_capacity(channels));
        frame.0.resize(channels, Signal::Unknown);
        frame
    }

    /// Create a new signal frame by copying from `source` `n` channels starting from index `i`.
    pub fn copy(source: &SignalFrame, i: usize, n: usize) -> SignalFrame {
        let mut frame = SignalFrame::new(n);
        frame.0[0..n].copy_from_slice(&source.0[i..i + n]);
        frame
    }

    pub fn fill(&mut self, signal: Signal) {
        self.0.fill(signal);
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn length(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn at(&self, i: usize) -> Signal {
        self.0[i]
    }

    pub fn set(&mut self, i: usize, signal: Signal) {
        self.0[i] = signal;
    }

    pub fn resize(&mut self, size: usize) {
        self.0.resize(size, Signal::Unknown);
    }
}

/// Signal routing information. This is a dumping ground for signal routing
/// functionality.
#[derive(Clone)]
pub enum Routing {
    /// Conservative routing: every input influences every output nonlinearly with extra latency in samples.
    Arbitrary(f64),
    /// Split or multisplit semantics.
    Split,
    /// Join or multijoin semantics.
    Join,
    /// Reverse channel order semantics. Equal number of inputs and outputs.
    Reverse,
    /// Generator with latency in samples.
    Generator(f64),
}

impl Routing {
    /// Routes signals from input to output.
    pub fn route(&self, input: &SignalFrame, outputs: usize) -> SignalFrame {
        let mut output = SignalFrame::new(outputs);

        // `Generator` is handled before the empty-input guard, and that
        // ordering is the whole point: a generator has no inputs, so the guard
        // below made this arm unreachable for exactly the nodes it exists for.
        // `noise`, `envelope`, `wave`, `sequencer`, `shared` and `ring` all
        // declare `Generator` with `inputs() == 0`, and every one of them was
        // getting `Unknown`. It went unnoticed because they all pass `0.0`, so
        // the wrong answer and the right one were the same number — the first
        // generator to declare a non-zero latency would have had it silently
        // dropped.
        if let Routing::Generator(latency) = self {
            for i in 0..outputs {
                output.set(i, Signal::Latency(*latency));
            }
            return output;
        }

        // Every remaining variant reads `input`, and all of them index or
        // divide by its length.
        if input.is_empty() {
            return output;
        }

        match self {
            Routing::Arbitrary(latency) => {
                // Fold the inputs first, then add this node's latency **once**.
                //
                // Adding `*latency` inside the fold — as `distort(*latency)`
                // plus a `combine_nonlinear(.., *latency)` per remaining input
                // — added it once per input, and made the result depend on the
                // *order* of the input channels: `combine_nonlinear` takes a
                // `min`, so an already-inflated running total was being
                // compared against a raw input latency. With sources at 0 and
                // 10 and a node latency of 5, `[0, 10]` gave 10 and `[10, 0]`
                // gave 5. A node's reported latency cannot depend on the order
                // its edges were wired.
                let mut combo = input.at(0).distort(0.0);
                for i in 1..input.len() {
                    combo = combo.combine_nonlinear(input.at(i), 0.0);
                }
                output.fill(combo.distort(*latency));
            }
            Routing::Split => {
                for i in 0..outputs {
                    output.set(i, input.at(i % input.len()));
                }
            }
            Routing::Join => {
                // Two shapes used to panic here, both reachable through
                // `AudioUnit::latency` / `AudioUnit::response`: zero outputs
                // divided by zero, and more outputs than inputs made the
                // integer division 0 and then indexed past the end. Every
                // other variant answers an empty frame for a degenerate
                // shape, so `Join` was alone in turning "sum these channels
                // into nothing" into a crash.
                if outputs == 0 {
                    return output;
                }
                // How many inputs for each output.
                let bundle = input.len() / outputs;
                for i in 0..outputs {
                    // Fewer inputs than outputs: the outputs past the end have
                    // nothing to sum and keep the `Unknown` they were built
                    // with, which is the honest answer rather than a wrapped
                    // duplicate. `i` only grows, so nothing later qualifies.
                    if i >= input.len() {
                        break;
                    }
                    let mut combo = input.at(i);
                    for j in 1..bundle {
                        combo = combo.combine_linear(
                            input.at(i + j * outputs),
                            0.0,
                            |x, y| x + y,
                            |x, y| x + y,
                        );
                    }
                    // Normalize. This is done to make join an inverse of split.
                    output.set(i, combo.scale(outputs as f64 / input.len() as f64));
                }
            }
            Routing::Reverse => {
                // Reached only for a non-empty input, deliberately. An empty
                // frame means "no signal information available", which is what
                // the guard above answers with a frame of `Unknown` — not a
                // width mismatch. Widening this to fire on the empty case
                // would put a new panic on the graph-commit path to report
                // something that is not a miswiring.
                assert_eq!(input.len(), outputs);
                for i in 0..outputs {
                    output.set(i, input.at(input.len() - 1 - i));
                }
            }
            // Handled above, before the empty-input guard.
            Routing::Generator(_) => unreachable!(),
        }
        output
    }
}
