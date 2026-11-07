//! Lookup cost by map size and shape. Run in release:
//! `cargo run --release -p tutti-tempo --example bench`.
//!
//! Numbers depend on the machine and on what else it is running; compare runs
//! only on a quiet one.

use std::hint::black_box;
use std::time::Instant;
use tutti_tempo::{CellGrid, Cursor, Shape, TempoMap, TempoPoint, chord_cell};
use tutti_types::{Beat, Bpm, Frame, SampleRate};

const RATE: SampleRate = SampleRate(48_000.0);
const ITERS: u64 = 2_000_000;

fn per_call(mut f: impl FnMut(u64) -> f64) -> f64 {
    let t = Instant::now();
    let mut acc = 0.0;
    for i in 0..ITERS {
        acc += f(i);
    }
    black_box(acc);
    t.elapsed().as_nanos() as f64 / ITERS as f64
}

fn main() {
    let grid = CellGrid::of(128);
    for n in [1usize, 100, 10_000] {
        for shape in [Shape::Step, Shape::RampTime, Shape::RampBeats] {
            let map = TempoMap::new((0..n).map(|i| TempoPoint {
                beat: Beat(i as f64 * 4.0),
                tempo: Bpm(80.0 + (i % 7) as f64 * 10.0),
                shape: if i + 1 == n { Shape::Step } else { shape },
            }))
            .expect("a valid map");
            let total = map
                .frame_at_or_after(Beat(n as f64 * 4.0), RATE)
                .get()
                .max(1);
            let mut cursor = Cursor::default();
            let random = per_call(|i| map.beat_at_frame(Frame(i * 977 % total), RATE).get());
            let hinted = per_call(|i| {
                map.beat_at_seconds_with((i * 3 % total) as f64 / RATE.get(), &mut cursor)
                    .get()
            });
            let inverse = per_call(|i| {
                map.frame_at_or_after(Beat((i % (n as u64 * 4000)) as f64 / 1000.0), RATE)
                    .get() as f64
            });
            let cell = per_call(|i| {
                chord_cell(&map, Frame(i * 977 % total), grid, RATE)
                    .tempo
                    .get()
            });
            println!(
                "n={n:>6} {shape:?}: beat_at_frame {random:5.1} ns, hinted beat_at_seconds {hinted:5.1} ns, \
                 frame_at_or_after {inverse:5.1} ns, chord_cell {cell:5.1} ns"
            );
        }
    }
}
