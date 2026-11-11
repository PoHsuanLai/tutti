use super::*;
use crate::render::Replay;
use tutti_types::{ChannelLayout, InterleavedMut};

const STEREO: ChannelLayout = ChannelLayout::STEREO;

/// `frames` stereo frames whose every sample is distinct and exact in `f32`:
/// the left of frame `n` is `n` and the right `-n`, scaled by a power of two.
fn frames(from: usize, frames: usize) -> Vec<f32> {
    (from..from + frames)
        .flat_map(|n| [n as f32 / 1024.0, -(n as f32) / 1024.0])
        .collect()
}

/// Everything `replay` hands out, read `block` frames at a time.
fn read_all(spill: &Spill, block: usize) -> Vec<f32> {
    let mut replay = spill.replay().unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; block * 2];
    loop {
        let n = replay.fill(InterleavedMut::new(&mut buf, STEREO)).unwrap();
        if n == 0 {
            return out;
        }
        out.extend_from_slice(&buf[..n * 2]);
    }
}

fn listing(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// A spill inside its budget stays in memory, creates no file, and reads back
/// what was pushed, in blocks of any size (a size that divides nothing here,
/// and one past the whole spill), more than once.
#[test]
fn a_spill_within_the_budget_stays_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.wav");
    let mut budget = SpillBudget::new(1 << 20);
    let mut spill = Spill::new(STEREO, &target);
    let mut want = Vec::new();
    for from in (0..1000).step_by(100) {
        let block = frames(from, 100);
        spill.push(&block, &mut budget).unwrap();
        want.extend_from_slice(&block);
    }
    spill.finish().unwrap();
    assert!(!spill.is_file());
    assert_eq!(spill.frames(), Samples(1000));
    assert!(listing(dir.path()).is_empty(), "no file beside the target");
    for block in [7, 100, 5000] {
        assert_eq!(read_all(&spill, block), want, "blocks of {block}");
    }
}

/// Past the budget a spill moves to a file beside its target (a dot-file named
/// for it, in the target's directory), keeps every frame in order across the
/// move, reads back the same whatever the block size and however many times,
/// and removes the file when it is dropped. The memory it held goes back to the
/// budget.
#[test]
fn a_spill_past_the_budget_moves_to_a_file_beside_the_target_and_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("out.wav");
    // Room for two blocks of 100 stereo frames (800 bytes each), not three.
    let mut budget = SpillBudget::new(2 * 800);
    let mut spill = Spill::new(STEREO, &target);
    let mut want = Vec::new();
    for from in (0..1000).step_by(100) {
        let block = frames(from, 100);
        spill.push(&block, &mut budget).unwrap();
        want.extend_from_slice(&block);
    }
    spill.finish().unwrap();
    assert!(spill.is_file());
    assert_eq!(budget.held, 0, "the memory it held went back to the budget");
    assert_eq!(spill.frames(), Samples(1000));
    let files = listing(dir.path());
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(
        files[0].starts_with(".out.wav.") && files[0].ends_with(".spill"),
        "{files:?}"
    );
    for block in [7, 64, 100, 5000] {
        assert_eq!(read_all(&spill, block), want, "blocks of {block}");
    }
    drop(spill);
    assert!(listing(dir.path()).is_empty(), "the spill file was removed");
}

/// The budget is shared: a spill that has taken all of it leaves the next one
/// nothing, so the second moves to a file at its first block while the first
/// stays in memory, and both read back everything.
#[test]
fn the_budget_is_shared_between_spills() {
    let dir = tempfile::tempdir().unwrap();
    let mut budget = SpillBudget::new(4 * 800);
    let mut a = Spill::new(STEREO, &dir.path().join("a.wav"));
    let mut b = Spill::new(STEREO, &dir.path().join("b.wav"));
    let (mut want_a, mut want_b) = (Vec::new(), Vec::new());
    for from in (0..400).step_by(100) {
        let block = frames(from, 100);
        a.push(&block, &mut budget).unwrap();
        want_a.extend_from_slice(&block);
    }
    for from in (0..400).step_by(100) {
        let block = frames(from + 5000, 100);
        b.push(&block, &mut budget).unwrap();
        want_b.extend_from_slice(&block);
    }
    a.finish().unwrap();
    b.finish().unwrap();
    assert_eq!((a.is_file(), b.is_file()), (false, true));
    assert_eq!(read_all(&a, 64), want_a);
    assert_eq!(read_all(&b, 64), want_b);
}
