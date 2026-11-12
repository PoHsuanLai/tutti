//! The session's spawner (design 029 §3.4): work that blocks (a file load, a
//! plugin round trip, a recording's pump) runs on a thread the host's
//! spawner gives it, and what it reports lands on the control thread in
//! [`Session::update`], in the order it was sent.
//!
//! A job borrows nothing of the session. It reports through its
//! [`Landings`], which never blocks, and ends exactly once, after every
//! message it sent: with its result, or with a [`JobFailed`] when it
//! panicked or the spawner dropped it unrun. Nothing cancels a job: one
//! that can stop carries its own flag, and [`Landings::is_closed`] tells it
//! when the session is gone, so it stops writing what no one will land.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the spawner's first users are the take's pump (doc 030 step 8) and the MIDI \
                  file and SoundFont loads (doc 017 L1f-b), which land on it next"
    )
)]

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::{Session, SessionEvent};

/// One unit of blocking work the session hands to its spawner
/// ([`SessionBuilder::spawn_with`](crate::SessionBuilder::spawn_with)). The
/// spawner calls [`run`](Self::run) on a thread of its own; a job it drops
/// unrun lands as [`JobFailed::Dropped`].
#[must_use = "a job that is neither run nor kept lands as dropped"]
pub struct Job {
    work: Option<Box<dyn FnOnce() + Send>>,
    /// Where an unrun job reports its end.
    end: Option<(Sender<Arrival>, JobId)>,
}

impl Job {
    /// Runs the work on the calling thread, blocking until it ends. A panic
    /// in it is caught and lands as [`JobFailed::Panicked`].
    pub fn run(mut self) {
        self.end = None;
        if let Some(work) = self.work.take() {
            work();
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some((tx, id)) = self.end.take() {
            // The session may be gone; then nothing waits for this end.
            let _ = tx.send(Arrival::End(id, Err(JobFailed::Dropped)));
        }
    }
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job").finish_non_exhaustive()
    }
}

/// Why a job ended without a result.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum JobFailed {
    /// The work panicked; `message` is the panic's, when it was a string.
    #[error("a session job panicked: {message}")]
    Panicked {
        /// The panic's message, or a placeholder for a payload of another
        /// type.
        message: String,
    },
    /// The spawner dropped the job without running it (a pool shut down, a
    /// thread that could not start).
    #[error("the spawner dropped a session job without running it")]
    Dropped,
}

/// The sending half a streaming job reports through. Sending never blocks
/// and never fails the work: after the session is gone a message is
/// returned to the sender and dropped.
pub(crate) struct Landings<M> {
    tx: Sender<Arrival>,
    id: JobId,
    closed: Arc<AtomicBool>,
    _message: PhantomData<fn(M)>,
}

impl<M: Send + 'static> Landings<M> {
    /// Queues `message` to land in the next [`Session::update`], after
    /// every message sent before it.
    ///
    /// # Errors
    ///
    /// The message back, when the session is gone.
    pub(crate) fn send(&self, message: M) -> Result<(), M> {
        if self.is_closed() {
            return Err(message);
        }
        self.tx
            .send(Arrival::Message(self.id, Box::new(message)))
            .map_err(|refused| match refused.0 {
                Arrival::Message(_, boxed) => {
                    *boxed.downcast::<M>().expect("the message this call boxed")
                }
                Arrival::End(..) => unreachable!("this call sent a message"),
            })
    }

    /// The session is gone: nothing sent will land, and the job's result
    /// will not either.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// What a streaming job's lander receives: each message in order, then its
/// end, exactly once.
#[derive(Debug)]
pub(crate) enum Landed<M, R> {
    /// A message the work sent.
    Message(M),
    /// The work's result, after every message it sent.
    Done(R),
    /// The work did not finish, after every message it sent.
    Failed(JobFailed),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct JobId(u64);

/// What crosses from a worker to the control thread.
enum Arrival {
    Message(JobId, Box<dyn Any + Send>),
    End(JobId, Result<Box<dyn Any + Send>, JobFailed>),
}

/// A job's lander, typed away: it downcasts what its own work sent.
type Lander = Box<dyn FnMut(&mut Session, Arrival, &mut Vec<SessionEvent>) + Send>;

/// The spawner a host gives the session.
#[derive(Clone)]
pub(crate) struct Spawner(Arc<dyn Fn(Job) + Send + Sync>);

impl Spawner {
    pub(crate) fn new(spawn: impl Fn(Job) + Send + Sync + 'static) -> Self {
        Self(Arc::new(spawn))
    }
}

/// The default spawner: a named `std::thread` per job. A thread that cannot
/// start drops its job, which lands as [`JobFailed::Dropped`].
impl Default for Spawner {
    fn default() -> Self {
        Self::new(|job: Job| {
            // A spawn that fails drops its closure, and the job in it.
            let _ = std::thread::Builder::new()
                .name("tutti-job".into())
                .spawn(move || job.run());
        })
    }
}

impl fmt::Debug for Spawner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Spawner").finish_non_exhaustive()
    }
}

/// The session's jobs: the spawner, the channel their arrivals come back
/// on, and the lander of each job still running.
pub(crate) struct Jobs {
    spawn: Spawner,
    tx: Sender<Arrival>,
    rx: Receiver<Arrival>,
    landers: HashMap<JobId, Lander>,
    next: u64,
    closed: Arc<AtomicBool>,
}

impl fmt::Debug for Jobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jobs")
            .field("running", &self.landers.len())
            .finish_non_exhaustive()
    }
}

impl Jobs {
    pub(crate) fn new(spawn: Spawner) -> Self {
        let (tx, rx) = channel();
        Self {
            spawn,
            tx,
            rx,
            landers: HashMap::new(),
            next: 0,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl Session {
    /// Runs `work` on the spawner and lands its result in a later
    /// [`update`](Self::update), on the control thread.
    pub(crate) fn submit<R, W, L>(&mut self, work: W, land: L)
    where
        R: Send + 'static,
        W: FnOnce() -> R + Send + 'static,
        L: FnOnce(&mut Session, Result<R, JobFailed>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        let mut land = Some(land);
        self.submit_stream(
            move |_: &Landings<std::convert::Infallible>| work(),
            move |session, landed, events| {
                let end = match landed {
                    Landed::Message(never) => match never {},
                    Landed::Done(result) => Ok(result),
                    Landed::Failed(failed) => Err(failed),
                };
                if let Some(land) = land.take() {
                    land(session, end, events);
                }
            },
        );
    }

    /// Runs `work` on the spawner, landing each message it sends through
    /// its [`Landings`] and then its end, exactly once and after every
    /// message, in later [`update`](Self::update)s on the control thread.
    /// `work` sees the landings by reference, so nothing it sends can
    /// outlive it and follow the end.
    pub(crate) fn submit_stream<M, R, W, L>(&mut self, work: W, mut land: L)
    where
        M: Send + 'static,
        R: Send + 'static,
        W: FnOnce(&Landings<M>) -> R + Send + 'static,
        L: FnMut(&mut Session, Landed<M, R>, &mut Vec<SessionEvent>) + Send + 'static,
    {
        let jobs = &mut self.jobs;
        let id = JobId(jobs.next);
        jobs.next += 1;
        let landings = Landings {
            tx: jobs.tx.clone(),
            id,
            closed: Arc::clone(&jobs.closed),
            _message: PhantomData,
        };
        let lander: Lander = Box::new(move |session, arrival, events| {
            let landed = match arrival {
                Arrival::Message(_, message) => Landed::Message(
                    *message
                        .downcast::<M>()
                        .expect("a job's messages are its own type"),
                ),
                Arrival::End(_, Ok(result)) => Landed::Done(
                    *result
                        .downcast::<R>()
                        .expect("a job's result is its own type"),
                ),
                Arrival::End(_, Err(failed)) => Landed::Failed(failed),
            };
            land(session, landed, events);
        });
        jobs.landers.insert(id, lander);
        let end_tx = jobs.tx.clone();
        let job = Job {
            work: Some(Box::new(move || {
                let end = catch_unwind(AssertUnwindSafe(|| work(&landings)))
                    .map(|result| Box::new(result) as Box<dyn Any + Send>)
                    .map_err(|payload| JobFailed::Panicked {
                        message: panic_message(payload.as_ref()),
                    });
                // After every message: they went down the same channel first.
                let _ = landings.tx.send(Arrival::End(id, end));
            })),
            end: Some((end_tx, id)),
        };
        (self.jobs.spawn.0)(job);
    }
}

impl Session {
    /// Lands what the jobs sent since the last update, in arrival order.
    /// What arrives during a lander (a job it submits that ends at once)
    /// waits for the next update.
    pub(crate) fn land_jobs(&mut self, out: &mut Vec<SessionEvent>) {
        let arrived: Vec<Arrival> = self.jobs.rx.try_iter().collect();
        for arrival in arrived {
            let (id, ends) = match &arrival {
                Arrival::Message(id, _) => (*id, false),
                Arrival::End(id, _) => (*id, true),
            };
            // Out of the table while it runs: a lander may submit a job.
            let Some(mut lander) = self.jobs.landers.remove(&id) else {
                continue;
            };
            lander(self, arrival, out);
            if !ends {
                self.jobs.landers.insert(id, lander);
            }
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a payload that is not a string".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::mpsc::sync_channel;
    use std::time::{Duration, Instant};
    use tutti_types::SampleRate;

    fn session(builder: crate::SessionBuilder) -> Session {
        builder.build_detached(SampleRate(48_000.0)).unwrap()
    }

    /// Updates until `done` holds, or panics after a generous wait.
    fn update_until(s: &mut Session, done: impl Fn() -> bool) -> Vec<SessionEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        while !done() {
            assert!(Instant::now() < deadline, "the job never landed");
            events.extend(s.update());
            std::thread::yield_now();
        }
        events
    }

    #[test]
    fn a_result_lands_in_update_on_the_control_thread_not_before() {
        let mut s = session(Session::builder());
        let (go, wait) = sync_channel::<()>(0);
        let landed = Arc::new(Mutex::new(None));
        let into = Arc::clone(&landed);
        s.submit(
            move || {
                wait.recv().unwrap();
                std::thread::current().id()
            },
            move |_, result, _| {
                *into.lock().unwrap() = Some((result.unwrap(), std::thread::current().id()));
            },
        );
        let _ = s.update();
        assert!(
            landed.lock().unwrap().is_none(),
            "nothing lands before the work ends"
        );
        go.send(()).unwrap();
        let _ = update_until(&mut s, || landed.lock().unwrap().is_some());
        let (worker, lander) = landed.lock().unwrap().unwrap();
        assert_ne!(worker, lander, "the work ran on the spawner's thread");
        assert_eq!(lander, std::thread::current().id(), "it landed in update");
    }

    #[test]
    fn messages_land_in_order_and_the_end_once_after_every_one() {
        let mut s = session(Session::builder());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&seen);
        s.submit_stream(
            |landings: &Landings<u32>| {
                for n in 0..500 {
                    landings.send(n).unwrap();
                }
                500
            },
            move |_, landed, _| {
                into.lock().unwrap().push(match landed {
                    Landed::Message(n) => n,
                    Landed::Done(end) => end + 1_000,
                    Landed::Failed(failed) => panic!("{failed}"),
                });
            },
        );
        let _ = update_until(&mut s, || seen.lock().unwrap().last() == Some(&1_500));
        let _ = s.update();
        let mut want: Vec<u32> = (0..500).collect();
        want.push(1_500);
        assert_eq!(*seen.lock().unwrap(), want);
    }

    #[test]
    fn a_panic_lands_as_failed_with_its_message() {
        let mut s = session(Session::builder());
        let landed = Arc::new(Mutex::new(None));
        let into = Arc::clone(&landed);
        s.submit(
            || -> u8 { panic!("the file was gone") },
            move |_, result, _| *into.lock().unwrap() = Some(result),
        );
        let _ = update_until(&mut s, || landed.lock().unwrap().is_some());
        assert_eq!(
            landed.lock().unwrap().take().unwrap(),
            Err(JobFailed::Panicked {
                message: "the file was gone".into()
            })
        );
    }

    #[test]
    fn a_job_the_spawner_drops_lands_as_dropped_and_its_work_never_runs() {
        let mut s = session(Session::builder().spawn_with(drop));
        let ran = Arc::new(AtomicBool::new(false));
        let landed = Arc::new(Mutex::new(None));
        let (r, into) = (Arc::clone(&ran), Arc::clone(&landed));
        s.submit(
            move || r.store(true, Ordering::SeqCst),
            move |_, result, _| *into.lock().unwrap() = Some(result),
        );
        let _ = s.update();
        assert_eq!(landed.lock().unwrap().take(), Some(Err(JobFailed::Dropped)));
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[test]
    fn the_hosts_spawner_runs_every_job() {
        let spawned = Arc::new(Mutex::new(0_usize));
        let count = Arc::clone(&spawned);
        let mut s = session(Session::builder().spawn_with(move |job| {
            *count.lock().unwrap() += 1;
            job.run();
        }));
        let landed = Arc::new(Mutex::new(Vec::new()));
        for n in 0..3_u8 {
            let into = Arc::clone(&landed);
            s.submit(
                move || n,
                move |_, r, _| into.lock().unwrap().push(r.unwrap()),
            );
        }
        let _ = s.update();
        assert_eq!(*spawned.lock().unwrap(), 3);
        assert_eq!(*landed.lock().unwrap(), [0, 1, 2]);
    }

    #[test]
    fn a_job_outliving_its_session_sees_it_closed_and_its_sends_come_back() {
        let mut s = session(Session::builder());
        let (go, wait) = sync_channel::<()>(0);
        let (opened, open) = sync_channel::<bool>(1);
        let (report, seen) = sync_channel::<(Result<(), u8>, bool)>(1);
        s.submit_stream(
            move |landings: &Landings<u8>| {
                opened.send(landings.is_closed()).unwrap();
                wait.recv().unwrap();
                report
                    .send((landings.send(7), landings.is_closed()))
                    .unwrap();
            },
            |_, _, _| {},
        );
        let wait_long = Duration::from_secs(10);
        assert!(
            !open.recv_timeout(wait_long).unwrap(),
            "open while the session lived"
        );
        drop(s);
        go.send(()).unwrap();
        let (sent, after) = seen.recv_timeout(wait_long).unwrap();
        assert_eq!(sent, Err(7), "the message comes back");
        assert!(after);
    }
}
