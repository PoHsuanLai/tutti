//! Which thread plays the CLAP `[main-thread]` and `[audio-thread]` roles.
//!
//! A plugin asks the host (`clap.thread-check`) which thread a call is on,
//! and the audio-side entries of one plugin must never overlap. Both are one
//! object's job: the claim on the `[audio-thread]` role is a lock, and holding
//! it publishes the claiming thread's identity.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::ThreadId;

use tutti_plugin_types::MainThread;

/// The thread roles of one plugin instance.
#[derive(Debug)]
pub struct ThreadRoles {
    main: ThreadId,
    // An `ArcSwapOption<ThreadId>` would not do: `store` waits for readers and
    // can run the retired `Arc`'s deallocation on the audio thread, and a
    // one-slot `Arc` cache is evicted whenever a GUI thread claims the role
    // between two blocks. A hash in an atomic allocates nothing.
    audio: AtomicU64,
    lock: Mutex<()>,
}

const NO_AUDIO_THREAD: u64 = 0;

fn thread_id_hash(id: ThreadId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    // Fold the sentinel away so a real thread can never be read as "unclaimed".
    match hasher.finish() {
        NO_AUDIO_THREAD => 1,
        h => h,
    }
}

/// RAII claim on the `[audio-thread]` role for one plugin instance.
///
/// While alive it holds the instance's role lock and has published the
/// claiming thread, so [`ThreadRoles::is_audio_thread`] answers `true` on
/// this thread and `false` everywhere else, and
/// [`ThreadRoles::is_main_thread`] answers `false` on this thread even when it
/// is the OS main thread (the spec lets a host drive `start_processing` from
/// there). Not re-entrant: claiming twice on one thread deadlocks.
#[derive(Debug)]
pub struct AudioClaim<'a> {
    roles: &'a ThreadRoles,
    // Held for the claim's lifetime; declared last so it releases only after
    // `Drop` has cleared the published identity.
    _guard: MutexGuard<'a, ()>,
}

impl Drop for AudioClaim<'_> {
    fn drop(&mut self) {
        // One store of a `u64`: nothing to free, so this cannot deallocate on
        // the audio thread. `Release` pairs with the `Acquire` in
        // `is_audio_thread`.
        self.roles.audio.store(NO_AUDIO_THREAD, Ordering::Release);
    }
}

impl ThreadRoles {
    /// Roles with the calling thread as the main thread and no audio claim.
    /// The token proves the caller is on the process's main thread, so the
    /// thread recorded here is the one every `&MainThread` entry runs on: this
    /// is the one place the main thread is named, and the `thread-check`
    /// answer, the host shims and `Plugin`'s drop all read it.
    #[must_use]
    pub fn new(_main: &MainThread) -> Self {
        Self {
            main: std::thread::current().id(),
            audio: AtomicU64::new(NO_AUDIO_THREAD),
            lock: Mutex::new(()),
        }
    }

    /// The thread that built these roles.
    #[must_use]
    pub fn main_thread_id(&self) -> ThreadId {
        self.main
    }

    /// Claim the `[audio-thread]` role, waiting until any other claim has
    /// been released. A poisoned lock is taken as is: the guarded value is
    /// `()`, so a panicking claimant broke no invariant.
    pub fn claim(&self) -> AudioClaim<'_> {
        let guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        // `Release` pairs with the `Acquire` in `is_audio_thread`.
        self.audio.store(
            thread_id_hash(std::thread::current().id()),
            Ordering::Release,
        );
        AudioClaim {
            roles: self,
            _guard: guard,
        }
    }

    /// Whether the calling thread holds the audio claim.
    #[must_use]
    pub fn is_audio_thread(&self) -> bool {
        let claimed = self.audio.load(Ordering::Acquire);
        claimed != NO_AUDIO_THREAD && claimed == thread_id_hash(std::thread::current().id())
    }

    /// Whether the calling thread is the main thread and holds no audio
    /// claim. Exclusive with [`is_audio_thread`](Self::is_audio_thread), per
    /// thread: a claim held by another thread does not demote this one.
    #[must_use]
    pub fn is_main_thread(&self) -> bool {
        std::thread::current().id() == self.main && !self.is_audio_thread()
    }
}
