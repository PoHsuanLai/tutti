//! The SVF, the EQ band and the ladder on the stream grid (doc 024 Q27): the render is a
//! function of the stream, not of how it is cut into blocks, and a control reaches
//! the render within one 16-frame segment of the grid point that sampled it.
//!
//! Both are analytic: the first renders one signal at several block lengths and
//! asks for the same bits; the second reads the filter's own held responses (an
//! impulse into a filter with no state answers with its first coefficient set, so
//! the output at the impulse is the coefficient set in force there).

use tutti_graph::contract::{drive_input, prepared};
use tutti_graph::{Env, Node, ParamNode, PlayClock, Transport, TransportChanges};
use tutti_nodes::{EqBandNode, LadderFilterNode, LadderType, SvfControls, SvfFilterNode, SvfType};
use tutti_types::{ChannelLayout, Db, Frame, Hz, Q, Resonance, SampleRate, Samples, UnitParam};

const RATE: SampleRate = SampleRate(48_000.0);
const FRAMES: usize = 2_048;
const BLOCKS: [usize; 4] = [1, 17, 64, 512];

fn noise(seed: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    (0..FRAMES)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            0.8 * ((state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0)
        })
        .collect()
}

/// `node`, prepared for 512-frame blocks, fed `inputs` in blocks of `block` with
/// `lanes` (each a function of the absolute frame, in port order) cut to match.
fn render<N: Node + ParamNode + 'static>(
    node: N,
    block: usize,
    inputs: &[Vec<f32>],
    lanes: &[Option<Vec<f32>>],
    act: impl Fn(&tutti_graph::ParamSet),
) -> Vec<Vec<f32>> {
    let mut node = prepared(node, RATE, 512);
    act(&node.param_set());
    let len = inputs[0].len();
    let mut out = vec![Vec::new(); inputs.len()];
    for start in (0..len).step_by(block) {
        let end = (start + block).min(len);
        let ins: Vec<&[f32]> = inputs.iter().map(|c| &c[start..end]).collect();
        let params: Vec<Option<&[f32]>> = lanes
            .iter()
            .map(|l| l.as_ref().map(|l| &l[start..end]))
            .collect();
        let env = Env {
            frame: Frame(start as u64),
            sample_rate: RATE,
            block_len: Samples(end - start),
            transport: Transport::default(),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let driven = drive_input(&mut node as &mut dyn Node, &env, &ins, &params, &[]).audio;
        for (all, part) in out.iter_mut().zip(driven) {
            all.extend(part);
        }
    }
    out
}

/// An exponential sweep of `lo` to `hi` over the render, a function of the
/// absolute frame.
fn sweep(lo: f32, hi: f32) -> Vec<f32> {
    (0..FRAMES)
        .map(|i| lo * (hi / lo).powf(i as f32 / FRAMES as f32))
        .collect()
}

fn same_at_every_block(what: &str, make: impl Fn() -> Box<dyn Fn(usize) -> Vec<Vec<f32>>>) {
    let run = make();
    let whole = run(512);
    assert!(whole[0].iter().any(|s| s.abs() > 1e-3), "{what}: silent");
    for block in BLOCKS {
        assert!(
            run(block) == whole,
            "{what}: blocks of {block} differ from blocks of 512"
        );
    }
}

/// An SVF bell and an EQ band with a lane on every control it has, and the
/// other cells moved before the first block, render the same bits whether the
/// stream arrives in blocks of 1, 17, 64 or 512 frames.
///
/// Mutations (run): counting the grid from each block's start (the stream
/// phase in `Cadence::steps` forced to 0) fails this at the blocks of 1, 17 and
/// 64; sampling each lane at the end of its segment rather than its start
/// (`solve_at`) fails this and the lag test; `COEFF_INTERVAL = 32` fails only the
/// lag test (the grid is still the stream's).
#[test]
fn blocks_of_any_length_render_alike_with_lanes_and_moved_cells() {
    let inputs = [noise(1), noise(2)];
    same_at_every_block("svf bell, three lanes", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            let lanes = [
                Some(sweep(300.0, 9_000.0)),
                Some(sweep(0.6, 7.0)),
                Some(
                    (0..FRAMES)
                        .map(|i| 9.0 * (i as f32 * 0.01).sin())
                        .collect::<Vec<_>>(),
                ),
            ];
            render(
                SvfFilterNode::<f64>::with_controls(
                    ChannelLayout::STEREO,
                    SvfControls {
                        ty: SvfType::Bell,
                        freq: Hz(1_000.0),
                        q: Q(1.0),
                        gain: Db(3.0),
                    },
                ),
                block,
                &inputs,
                &lanes,
                |_| {},
            )
        })
    });
    same_at_every_block("eq band, a cutoff lane, gain and Q cells moved", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            render(
                EqBandNode::<f32>::new(SvfType::HighShelf, Hz(4_000.0), Q(0.8), Db(8.0)),
                block,
                &inputs[..1],
                &[Some(sweep(1_000.0, 8_000.0)), None, None],
                |set| {
                    // Before the first block: nothing to glide from, so the cells
                    // are the controls of the first solve at every block length.
                    assert!(set.set(UnitParam::GainDb, -5.0));
                    assert!(set.set(UnitParam::Q, 2.5));
                },
            )
        })
    });
}

/// A stereo ladder with a lane on its cutoff, its resonance and its drive, and one
/// with the drive cell moved before the first block under a cutoff lane, render the
/// same bits at blocks of 1, 17, 64 or 512 frames: the drive is on the grid like the
/// coefficients (a lane sampled per segment, a cell held).
///
/// Mutations (run): counting the grid from each block's start (the stream phase in
/// `Cadence::steps` forced to 0) fails both (blocks of 1, 17, 64 and 512 differ); reading the
/// drive lane 15 frames late (`s[at.frame + 15]` in `LadderParams::read`'s drive arm)
/// fails the first.
#[test]
fn the_ladder_renders_alike_at_every_block_length_with_lanes() {
    let inputs = [noise(4), noise(5)];
    same_at_every_block("ladder, cutoff, resonance and drive lanes", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            let lanes = [
                Some(sweep(200.0, 8_000.0)),
                Some(
                    (0..FRAMES)
                        .map(|i| 0.5 + 0.4 * (i as f32 * 0.013).sin())
                        .collect::<Vec<_>>(),
                ),
                Some(
                    (0..FRAMES)
                        .map(|i| 3.0 + 2.5 * (i as f32 * 0.007).sin())
                        .collect::<Vec<_>>(),
                ),
            ];
            render(
                LadderFilterNode::<f64>::with_channels(
                    ChannelLayout::STEREO,
                    LadderType::LP24,
                    Hz(1_000.0),
                    Resonance::new(0.3),
                ),
                block,
                &inputs,
                &lanes,
                |_| {},
            )
        })
    });
    same_at_every_block("ladder, a cutoff lane and the drive cell moved", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            render(
                LadderFilterNode::<f32>::new(LadderType::HP12, Hz(2_000.0), Resonance::new(0.4)),
                block,
                &inputs[..1],
                &[Some(sweep(500.0, 6_000.0)), None, None],
                |set| {
                    assert!(set.set(UnitParam::Drive, 4.0));
                },
            )
        })
    });
}

/// A step in a drive lane at frame 40 reaches the render within one segment of the
/// grid point that samples it, as a cutoff's does: the drive is on the grid.
///
/// The point at 32 reads the old drive and its segment ends on it at frame 47; the
/// point at 48 reads the new one and its segment ends on it at frame 63. An impulse
/// into a ladder with no state answers with the drive in force at that frame, so the
/// output there is the held filter's with that drive, bit for bit. (Drive was once
/// read per sample, outside the solve: a step at 40 would be in the render at 40.)
///
/// Mutations (run): sampling the drive lane 15 frames late (`s[at.frame + 15]`, the
/// segment's end) reads the new drive at 47 and fails the first assertion; solving each
/// step at its last frame (`solve_at` on the stream arm of `Cadence::steps`) fails it
/// too.
#[test]
fn a_drive_lane_step_is_in_the_render_within_one_segment() {
    const STEP: usize = 40;
    let (old, new) = (1.0f32, 6.0f32);
    let step: Vec<f32> = (0..64).map(|i| if i < STEP { old } else { new }).collect();
    let response = |drive: Option<f32>, at: usize| -> f64 {
        let mut x = vec![0.0f32; 64];
        x[at] = 0.5;
        let lane = drive.map_or_else(|| step.clone(), |d| vec![d; 64]);
        let node = LadderFilterNode::<f64>::new(LadderType::HP24, Hz(1_500.0), Resonance::new(0.2));
        let out = render(node, 64, &[x], &[None, None, Some(lane)], |_| {});
        f64::from(out[0][at])
    };
    let at_old = response(Some(old), 47);
    let at_new = response(Some(new), 63);
    assert_ne!(at_old, at_new, "the two drives must be told apart");
    assert_eq!(
        response(None, 47),
        at_old,
        "frame 47 is still on the old drive"
    );
    assert_eq!(response(None, 63), at_new, "frame 63 is on the new drive");
}

/// A cell moved before the first block (no lane) is held: every block length
/// renders the same bits.
///
/// Not sensitive to the grid mutations above (a held set has no ramp to cut);
/// it pins the path the lanes' case does not take, one retarget at the first
/// grid point and none after.
#[test]
fn held_cells_render_alike_at_every_block_length() {
    let inputs = [noise(3)];
    same_at_every_block("svf low-pass, cells", || {
        let inputs = inputs.clone();
        Box::new(move |block| {
            render(
                SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(800.0), Q(1.2)),
                block,
                &inputs,
                &[None, None, None],
                |set| {
                    assert!(set.set(UnitParam::Cutoff, 3_000.0));
                },
            )
        })
    });
}

/// A step in a cutoff lane at frame 40 reaches the render within one segment of
/// the grid point that samples it.
///
/// The grid points are 0, 16, 32, 48. The point at 32 reads the old cutoff and
/// its segment ends on that set at frame 47: an impulse there (into a filter with
/// no state) is answered as the old held filter answers it. The point at 48 is
/// the first to read the new cutoff and its segment ends on that set at frame 63,
/// so an impulse there is answered as the new held filter answers it: 16 frames
/// after the last point that did not see the step, 24 after the step itself.
///
/// Mutations (run): sampling the lane at the segment's last frame rather than its
/// first (`solve_at` on the stream arm of `Cadence::steps`) reads the new cutoff
/// at 47 and fails the first assertion; `COEFF_INTERVAL = 32` (a 32-frame grid and
/// ramp) leaves frame 47 and 63 on neither set and fails.
#[test]
fn a_lane_step_is_in_the_render_within_one_segment() {
    const STEP: usize = 40;
    let (old, new) = (400.0f32, 4_000.0f32);
    let lane: Vec<f32> = (0..64).map(|i| if i < STEP { old } else { new }).collect();
    let response = |cutoff: Option<f32>, at: usize| -> f64 {
        let mut x = vec![0.0f32; 64];
        x[at] = 1.0;
        let held = cutoff.map(|c| vec![c; 64]);
        let lanes = [Some(held.unwrap_or_else(|| lane.clone())), None, None];
        let node = SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(old), Q(0.707));
        let out = render(node, 64, &[x], &lanes, |_| {});
        f64::from(out[0][at])
    };
    let at_old = response(Some(old), 47);
    let at_new = response(Some(new), 63);
    assert_ne!(at_old, at_new, "the two cutoffs must be told apart");
    assert_eq!(
        response(None, 47),
        at_old,
        "frame 47 is still on the old set"
    );
    assert_eq!(response(None, 63), at_new, "frame 63 is on the new set");
}
