//! Lookup cost by map size and shape: `beat_at_frame` from a fresh view,
//! `beat_at_seconds` through a view's hint, `frame_at_or_after` and
//! `chord_cell`. Numbers depend on the machine and on what else it is
//! running; compare runs only on a quiet one.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tutti_tempo::{CellGrid, TempoMap, TempoPoint, Transition};
use tutti_types::{Beat, Bpm, Frame, SampleRate, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;

fn lookup(c: &mut Criterion) {
    let grid = CellGrid::of(128);
    let mut group = c.benchmark_group("lookup");
    for n in [1usize, 100, 10_000] {
        for shape in [
            Transition::Step,
            Transition::RampTime,
            Transition::RampBeats,
        ] {
            let map = TempoMap::new((0..n).map(|i| TempoPoint {
                beat: Beat(i as f64 * 4.0),
                tempo: Bpm::of(80.0 + (i % 7) as f64 * 10.0),
                transition: if i + 1 == n { Transition::Step } else { shape },
            }))
            .expect("a valid map");
            let total = map
                .view(RATE)
                .frame_at_or_after(Beat(n as f64 * 4.0))
                .get()
                .max(1);
            let id = |name: &str| BenchmarkId::new(name, format!("n={n} {shape:?}"));

            let mut i = 0u64;
            group.bench_function(id("beat_at_frame"), |b| {
                b.iter(|| {
                    i += 1;
                    black_box(
                        map.view(RATE)
                            .beat_at_frame(Frame::new(i * 977 % total))
                            .get(),
                    )
                })
            });
            let mut view = map.view(RATE);
            group.bench_function(id("hinted_beat_at_seconds"), |b| {
                b.iter(|| {
                    i += 1;
                    black_box(
                        view.beat_at_seconds(Seconds((i * 3 % total) as f64 / RATE.get()))
                            .get(),
                    )
                })
            });
            group.bench_function(id("frame_at_or_after"), |b| {
                b.iter(|| {
                    i += 1;
                    black_box(
                        map.view(RATE)
                            .frame_at_or_after(Beat((i % (n as u64 * 4000)) as f64 / 1000.0))
                            .get(),
                    )
                })
            });
            group.bench_function(id("chord_cell"), |b| {
                b.iter(|| {
                    i += 1;
                    black_box(
                        map.view(RATE)
                            .chord_cell(Frame::new(i * 977 % total), grid)
                            .tempo
                            .get(),
                    )
                })
            });
        }
    }
    group.finish();
}

criterion_group!(benches, lookup);
criterion_main!(benches);
