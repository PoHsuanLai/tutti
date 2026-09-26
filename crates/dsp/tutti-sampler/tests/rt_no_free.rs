//! Regression gate: the pool's command drain must not **free** on the audio
//! thread.
//!
//! `tests/rt_no_alloc.rs` guards allocation, and says why it cannot see a
//! free: `assert_no_alloc` counts allocations only. This file counts
//! deallocations, with a global allocator that tallies every `dealloc` made
//! while a thread-local flag is up, so the drain's frees are counted and
//! nothing else's (the test harness's own threads included).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{AudioUnit, Beat, Bpm, BufferVec, SampleRate, StretchFactor, Timeline};
use tutti_io::Wave;
use tutti_sampler::{MemorySource, Playback, SlotId, Voice, VoiceCommand, VoicePool, VoiceSource};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static FREES: Cell<usize> = const { Cell::new(0) };
}

struct CountFrees;

unsafe impl GlobalAlloc for CountFrees {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // `try_with`: a thread tearing down its locals still frees.
        let _ = WATCHING.try_with(|w| {
            if w.get() {
                let _ = FREES.try_with(|f| f.set(f.get() + 1));
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountFrees = CountFrees;

/// Frees made on this thread while `f` runs.
fn frees_during(f: impl FnOnce()) -> usize {
    FREES.with(|c| c.set(0));
    WATCHING.with(|w| w.set(true));
    f();
    WATCHING.with(|w| w.set(false));
    FREES.with(|c| c.get())
}

struct Clock {
    beat: AtomicU64,
    rolling: AtomicBool,
}

impl Timeline for Clock {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }
    fn tempo(&self) -> Bpm {
        Bpm::new(120.0)
    }
    fn is_rolling(&self) -> bool {
        self.rolling.load(Ordering::Relaxed)
    }
    fn segment_generation(&self) -> u64 {
        0
    }
}

/// **Adding voices, and replacing one, frees nothing in the drain.** Three
/// `AddVoice`s — two stretched (their filters built by the sender), then a
/// second add at the first one's id, which replaces that slot — are drained
/// by one `process`, counting frees on this thread. The replaced slot comes
/// back through the retirement channel, where the control thread frees it.
///
/// Mutation (run): the drain unboxing the voice (`insert_voice_inner(id,
/// Box::new(*voice), …)`, the pre-fix `*voice`) → the command's box is freed
/// in the drain → fails. Mutation (run): a replaced slot dropped in place
/// (`self.voices.retain(|s| s.id != id)`, the pre-fix line) → its filter and
/// box are freed in the drain → fails.
#[test]
fn the_add_voice_drain_frees_nothing() {
    let clock = Arc::new(Clock {
        beat: AtomicU64::new(0f64.to_bits()),
        rolling: AtomicBool::new(true),
    });
    let wave = Arc::new(Wave::from_samples(48_000.0, &vec![0.5f32; 48_000]));
    let (mut pool, handle) =
        VoicePool::with_transport(Arc::clone(&clock) as Arc<dyn Timeline>, None);
    pool.set_sample_rate(SampleRate(48_000.0));
    let voice = || {
        Box::new(Voice {
            source: VoiceSource::Memory(MemorySource::with_transport(
                Arc::clone(&wave),
                Arc::clone(&clock) as Arc<dyn Timeline>,
                Beat::new(0.0),
                None,
            )),
            play: Playback {
                stretch: StretchFactor::new(2.0),
                ..Playback::default()
            },
            channel_index: None,
        })
    };
    for id in [1, 2, 1] {
        handle
            .send(VoiceCommand::AddVoice {
                id: SlotId(id),
                voice: voice(),
                stretch: None,
            })
            .expect("the command queue has room");
    }
    let input = BufferVec::new(0);
    let mut output = BufferVec::new(2);
    let frees = frees_during(|| {
        pool.process(64, &input.buffer_ref(), &mut output.buffer_mut());
    });
    assert_eq!(pool.voice_count(), 2, "the third add replaced the first");
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread; a box or a \
         replaced slot must travel to the control thread instead"
    );
    assert_eq!(
        handle.collect_retired(),
        1,
        "the replaced slot is retired to the control thread"
    );
}
