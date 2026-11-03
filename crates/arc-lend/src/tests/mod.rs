//! Unit tests: they reach the crate's private state (a reader's slot, the
//! control block), so they live inside it, apart from the code they test.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;

use crate::slots::FREE;
use crate::sync::Ordering;
use crate::{ArcLend, Loan, Reader};

use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering as StdOrdering};
use std::sync::mpsc;
use std::time::Duration;

/// Records the thread that dropped it, so a test can say *where* a value was
/// freed, not just that it was.
struct DropSite {
    id: usize,
    sites: Arc<std::sync::Mutex<Vec<(usize, std::thread::ThreadId)>>>,
}
impl Drop for DropSite {
    fn drop(&mut self) {
        self.sites
            .lock()
            .unwrap()
            .push((self.id, std::thread::current().id()));
    }
}

type Sites = Arc<std::sync::Mutex<Vec<(usize, std::thread::ThreadId)>>>;

fn sites() -> (Sites, impl Fn(usize) -> Arc<DropSite>) {
    let sites: Sites = Arc::default();
    let make = {
        let sites = sites.clone();
        move |id| {
            Arc::new(DropSite {
                id,
                sites: sites.clone(),
            })
        }
    };
    (sites, make)
}

fn freed(sites: &Sites) -> Vec<usize> {
    sites.lock().unwrap().iter().map(|&(id, _)| id).collect()
}

/// Puts `reader`'s slot in the state a lend is in between its claim and its
/// announcement, as if the reader were preempted there: the first half of
/// `Reader::lend`, without the load or the announcement.
fn claim_only<T>(reader: &mut Reader<T>) {
    let claim = reader.next_claim();
    reader.slot().word.store(claim, Ordering::Release);
}

/// Clears a slot left claimed by [`claim_only`], as the rest of that lend and
/// its loan's drop would.
fn finish<T>(reader: &Reader<T>) {
    reader.slot().word.store(FREE, Ordering::Release);
}

impl<T> ArcLend<T> {
    /// How many slots the slot list has: the high-water mark of readers
    /// registered at once, rounded up to a block.
    fn slot_count(&self) -> usize {
        self.lock().slots.len()
    }
}

mod auto_traits;
mod claims;
mod publisher;
mod reclaim;
mod slots;
mod stress;
