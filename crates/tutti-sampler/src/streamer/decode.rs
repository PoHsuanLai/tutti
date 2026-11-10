//! The decode lane: whole-file decodes off the streamer's thread (doc 025
//! §2.5, decision 21).
//!
//! A file that cannot seek (mp3, ogg: no frame count, or a decoder that will
//! not seek) is decoded whole before its stream can exist, and that takes as
//! long as the file is. Done inside the command drain it stopped every other
//! stream's refill for its duration and back-pressured the caller's bounded
//! queue. It is one thread here, started on first use and fed one job at a
//! time (a burst of opens costs one core, never the streamer's), and its
//! result comes back through the streamer's own command queue as
//! [`Decoded`](super::command::StreamerCommand::Decoded), so the streamer
//! wakes for it as it wakes for any command.
//!
//! The thread is not joined: it ends after the job it is on when the lane is
//! dropped (the job queue disconnects), or when the streamer's queue is gone
//! (its send fails).

// With no codec feature there is no decode to run: the lane is never asked for
// a job, and `load_wave` reads only the cache. A feature-matrix artifact, so
// scoped to that build (as `metrics.rs` and `cache.rs` are).
#![cfg_attr(
    not(codec),
    allow(
        dead_code,
        reason = "the decode lane's only caller is the codec-gated whole-file arm of `begin`"
    )
)]

use std::any::Any;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::Sender;
use tutti_codec::Wave;

use super::StreamerSender;
use super::cache::LruCache;
use super::command::{Decoded, StreamId, StreamerCommand};
use super::handlers::Handles;
use super::metrics::{Metrics, ReadMeter};
use crate::OpenError;

/// How a thread is started: the seam a test replaces to see a spawn fail.
pub(super) type Spawner = fn(&str, Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>;

/// The OS's thread: what ships.
fn os_spawn(name: &str, body: Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.into()).spawn(body)
}

/// A hook a test runs where the streamer reads a file: the stream and the
/// frame it reads. It may block, or panic.
#[cfg(test)]
pub(super) type ReadHook = Arc<dyn Fn(StreamId, usize) + Send + Sync>;

/// A hook a test runs on the decode thread, before a file is decoded. It may
/// block, to hold a decode open.
#[cfg(test)]
pub(super) type DecodeHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// What a test can replace: how threads start, and what runs where the
/// streamer reads a file. Production is [`Seams::default`].
#[derive(Clone)]
pub(super) struct Seams {
    pub(super) spawn: Spawner,
    #[cfg(test)]
    pub(super) read_hook: Option<ReadHook>,
    #[cfg(test)]
    pub(super) decode_hook: Option<DecodeHook>,
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            spawn: os_spawn,
            #[cfg(test)]
            read_hook: None,
            #[cfg(test)]
            decode_hook: None,
        }
    }
}

/// Why `file_path` could not be turned into audio: the file itself when it
/// cannot be opened, else a format this build cannot decode.
pub(super) fn unreadable(file_path: &Path) -> OpenError {
    match std::fs::File::open(file_path) {
        Err(error) => OpenError::Io(error.kind()),
        Ok(_) => OpenError::Format,
    }
}

/// A whole-file decode asked of the lane.
struct Job {
    id: StreamId,
    path: PathBuf,
}

/// The decode thread's queue, started on first use. Streamer-thread-local.
pub(super) struct DecodeLane {
    /// The streamer's own queue, where a result is sent.
    reports: StreamerSender,
    /// The job queue, once the thread has started.
    jobs: Option<Sender<Job>>,
}

impl DecodeLane {
    pub(super) fn new(reports: StreamerSender) -> Self {
        Self {
            reports,
            jobs: None,
        }
    }

    /// Asks for `path` to be decoded whole for stream `id`, starting the thread
    /// if this is the first job. The answer arrives as a `Decoded` command.
    ///
    /// # Errors
    ///
    /// [`OpenError::Io`] when the thread cannot be started, so the open fails
    /// where it would have waited for good.
    pub(super) fn submit(
        &mut self,
        shared: &Handles,
        id: StreamId,
        path: PathBuf,
    ) -> Result<(), OpenError> {
        let job = Job { id, path };
        let job = match &self.jobs {
            Some(jobs) => match jobs.send(job) {
                Ok(()) => return Ok(()),
                // The thread is gone (a panic that escaped the job's own
                // containment): start another.
                Err(returned) => returned.0,
            },
            None => job,
        };
        let jobs = self.start(shared)?;
        let sent = jobs.send(job);
        self.jobs = Some(jobs);
        sent.map_err(|_| OpenError::Io(io::ErrorKind::BrokenPipe))
    }

    fn start(&self, shared: &Handles) -> Result<Sender<Job>, OpenError> {
        let (jobs, queue) = crossbeam_channel::unbounded::<Job>();
        let reports = self.reports.clone();
        let (cache, metrics) = (Arc::clone(&shared.cache), Arc::clone(&shared.metrics));
        #[cfg(test)]
        let decode_hook = shared.seams.decode_hook.clone();
        let body = move || {
            let mut meter = ReadMeter::new();
            for Job { id, path } in queue {
                // A panic in the decoder is this file's failure, not the
                // thread's: the next job runs.
                let wave = catch_unwind(AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if let Some(hook) = &decode_hook {
                        hook(&path);
                    }
                    load_wave(&cache, &metrics, &mut meter, &path)
                }));
                let wave = match wave {
                    Ok(Some(wave)) => Ok(wave),
                    Ok(None) => Err(unreadable(&path)),
                    Err(payload) => {
                        tracing::error!(
                            "decoding {} panicked: {}",
                            path.display(),
                            panic_message(payload.as_ref())
                        );
                        Err(OpenError::Format)
                    }
                };
                let cmd = StreamerCommand::Decoded {
                    id,
                    wave: Decoded(wave),
                };
                if reports.send(cmd).is_err() {
                    break;
                }
            }
        };
        (shared.seams.spawn)("tutti-decode", Box::new(body))
            .map_err(|error| OpenError::Io(error.kind()))?;
        Ok(jobs)
    }
}

/// The whole-file fallback: hand back a cached [`Wave`], or decode one and cache
/// it. `None` means the file could not be turned into audio.
///
/// With no codec feature enabled there is no decoder to call, so the miss arm is
/// gated out and a miss is simply `None`. The cache *lookup* stays live in both
/// builds: entries reach it from elsewhere, and a hit needs no decoder.
fn load_wave(
    cache: &LruCache,
    metrics: &Metrics,
    meter: &mut ReadMeter,
    file_path: &Path,
) -> Option<Arc<Wave>> {
    if let Some(cached) = cache.get(file_path) {
        return Some(cached);
    }

    // `Wave::read` is `tutti-codec`'s decode path, which is compiled only when a
    // codec feature is on — calling it unconditionally breaks the
    // `--no-default-features` build.
    #[cfg(codec)]
    {
        if let Ok(w) = Wave::read(file_path) {
            let arc_wave = Arc::new(w);
            let bytes = arc_wave.len() as u64 * arc_wave.channels() as u64 * 4;
            meter.record_read(bytes, metrics);
            cache.insert(file_path.to_path_buf(), arc_wave.clone());
            return Some(arc_wave);
        }
    }

    // Silences the unused-parameter warning in the codec-free build, where the
    // only way to reach this line is a cache miss with nothing able to fill it.
    #[cfg(not(codec))]
    let _ = (metrics, meter);

    None
}

/// What a panic said, for the log.
pub(super) fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("(not a string)")
}
