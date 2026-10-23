//! **What does one 64-frame block of binaural rendering cost?**
//!
//! A 64-frame block at 48 kHz has **1.333 ms**; divide a case's time by that
//! for the fraction of the budget one binaural source consumes. The
//! convolution runs once per 128 frames, so the mean over many blocks is
//! what this reports, not the worst block.
//!
//! The sphere is synthetic: a twice-subdivided icosahedron (162 vertices,
//! 320 faces, about the density of a measured set) with decaying-noise HRIRs
//! of 128 and 512 taps, at the node's rate so nothing is resampled.
//! `hrtf_static` holds the source still; `hrtf_moving` writes a new bearing
//! every block, which keeps the 50 ms de-zipper ramp, and so the crossfade
//! between filters, permanently in flight.
//!
//! The node is called by hand through `tutti_graph::contract::Direct` (no
//! graph around it, no allocation per block).

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::{Azimuth, Elevation, SampleRate};
use tutti_graph::contract::Direct;
use tutti_graph::Node;
use tutti_spatial::HrtfBinauralNode;

const BLOCK: usize = 64;
const RATE: u32 = 48_000;

/// A cheap LCG, so the bench needs no `rand`.
struct Lcg(u32);

impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

/// An icosahedron subdivided `levels` times, projected onto the unit sphere.
fn icosphere(levels: usize) -> (Vec<[f32; 3]>, Vec<[u32; 3]>) {
    let p = (1.0 + 5.0f32.sqrt()) / 2.0;
    let mut verts: Vec<[f32; 3]> = vec![
        [-1.0, p, 0.0],
        [1.0, p, 0.0],
        [-1.0, -p, 0.0],
        [1.0, -p, 0.0],
        [0.0, -1.0, p],
        [0.0, 1.0, p],
        [0.0, -1.0, -p],
        [0.0, 1.0, -p],
        [p, 0.0, -1.0],
        [p, 0.0, 1.0],
        [-p, 0.0, -1.0],
        [-p, 0.0, 1.0],
    ];
    let mut faces: Vec<[u32; 3]> = vec![
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ];
    let normalize = |v: [f32; 3]| {
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        [v[0] / n, v[1] / n, v[2] / n]
    };
    for v in &mut verts {
        *v = normalize(*v);
    }
    for _ in 0..levels {
        let mut cache = std::collections::HashMap::new();
        let mut midpoint = |a: u32, b: u32, verts: &mut Vec<[f32; 3]>| {
            *cache.entry((a.min(b), a.max(b))).or_insert_with(|| {
                let (va, vb) = (verts[a as usize], verts[b as usize]);
                verts.push(normalize([va[0] + vb[0], va[1] + vb[1], va[2] + vb[2]]));
                (verts.len() - 1) as u32
            })
        };
        let mut next = Vec::with_capacity(faces.len() * 4);
        for [a, b, c] in faces {
            let ab = midpoint(a, b, &mut verts);
            let bc = midpoint(b, c, &mut verts);
            let ca = midpoint(c, a, &mut verts);
            next.extend([[a, ab, ca], [b, bc, ab], [c, ca, bc], [ab, bc, ca]]);
        }
        faces = next;
    }
    (verts, faces)
}

/// The sphere's bytes in the HRIR format: decaying noise per vertex and ear.
fn sphere_bytes(ir_len: usize) -> Vec<u8> {
    let (verts, faces) = icosphere(2);
    let mut rng = Lcg(0x1234_5678);
    let mut b = Vec::new();
    b.extend_from_slice(b"HRIR");
    b.extend_from_slice(&RATE.to_le_bytes());
    b.extend_from_slice(&(ir_len as u32).to_le_bytes());
    b.extend_from_slice(&(verts.len() as u32).to_le_bytes());
    b.extend_from_slice(&((faces.len() * 3) as u32).to_le_bytes());
    for f in &faces {
        for idx in f {
            b.extend_from_slice(&idx.to_le_bytes());
        }
    }
    for v in &verts {
        for c in v {
            b.extend_from_slice(&c.to_le_bytes());
        }
        for _ear in 0..2 {
            for i in 0..ir_len {
                let decay = (-(i as f32) / (ir_len as f32 / 6.0)).exp();
                b.extend_from_slice(&(rng.next() * decay).to_le_bytes());
            }
        }
    }
    b
}

fn bench(c: &mut Criterion) {
    for moving in [false, true] {
        let name = if moving { "hrtf_moving" } else { "hrtf_static" };
        let mut g = c.benchmark_group(name);
        for ir_len in [128usize, 512] {
            g.throughput(Throughput::Elements(BLOCK as u64));
            let bytes = sphere_bytes(ir_len);
            let node = HrtfBinauralNode::new(&bytes, SampleRate(f64::from(RATE)))
                .expect("the sphere parses");
            node.set_position(Azimuth(20.0), Elevation(10.0));
            let mut d = Direct::new(node, SampleRate(f64::from(RATE)), BLOCK);
            // Seat the de-zipper on the position, so the static case renders
            // one filter from its first block.
            Node::reset(&mut d.node);
            let mut rng = Lcg(0x2545_f491);
            for ch in d.inputs_mut() {
                for x in ch.iter_mut() {
                    *x = rng.next();
                }
            }
            for _ in 0..64 {
                d.block();
            }
            let mut bearing = 0.0f32;
            g.bench_with_input(BenchmarkId::from_parameter(ir_len), &ir_len, |b, _| {
                b.iter(|| {
                    if moving {
                        bearing = if bearing > 170.0 {
                            -170.0
                        } else {
                            bearing + 7.0
                        };
                        d.node.set_position(Azimuth(bearing), Elevation(10.0));
                    }
                    d.block();
                    black_box(d.output(0)[BLOCK - 1]);
                })
            });
        }
        g.finish();
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
