//! A reader caught between its claim and its announcement.

use super::*;

/// The publisher never waits on a reader caught between claiming its slot
/// and announcing. It keeps everything that reader might still load (each
/// value current at or after the last scan before the claim) and checks
/// again on the next publish.
///
/// The publishes run on another thread with a deadline, so a publisher that
/// waited for the claim fails by timeout rather than hanging the suite.
#[test]
#[cfg_attr(miri, ignore = "waits on a wall-clock deadline")]
fn the_publisher_never_waits_on_a_reader_mid_lend() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut reader = cell.reader();
    claim_only(&mut reader);

    let (done_tx, done_rx) = mpsc::channel();
    let publisher = {
        let (cell, make) = (cell.clone(), make);
        std::thread::spawn(move || {
            for id in 1..=3 {
                cell.publish(make(id));
            }
            done_tx.send(()).unwrap();
            make
        })
    };
    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("publish waited on a reader mid-lend");
    let make = publisher.join().unwrap();

    // The claim was made before the first scan, so the reader may load
    // anything from value 0 on: nothing retired can go.
    assert_eq!(cell.retired_len(), 3);
    assert!(freed(&sites).is_empty());

    // Once the lend is done (and its loan dropped), the next publish frees
    // them all.
    finish(&reader);
    cell.publish(make(4));
    assert_eq!(freed(&sites), vec![0, 1, 2, 3]);
    assert_eq!(cell.retired_len(), 0);
}

/// A claim the publisher has already seen keeps the floor it had then: the
/// reader may have been preempted before its load ever since, so it can
/// still load the oldest value current since that scan.
#[test]
fn a_claim_seen_before_keeps_its_floor() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut reader = cell.reader();

    claim_only(&mut reader);
    cell.publish(make(1)); // sees the claim first: floor 0
    cell.publish(make(2)); // the same claim: still floor 0
    assert!(freed(&sites).is_empty(), "0 and 1 may still be loaded");
    assert_eq!(cell.retired_len(), 2);
    finish(&reader);
}

/// A *new* claim (a later generation) pins only from the last scan on: the
/// last scan read the slot before the claim, so the reader loads a value no
/// older than the one current then. This is what the generation in a claim
/// is for; without it a reader that keeps lending would look mid-lend
/// forever and pin every value retired since its first claim.
#[test]
fn a_new_claim_pins_only_from_the_last_scan() {
    let (sites, make) = sites();
    let cell = Arc::new(ArcLend::from_arc(make(0)));
    let mut reader = cell.reader();

    claim_only(&mut reader);
    cell.publish(make(1)); // floor 0: keeps 0

    // The reader finishes that lend, drops it, and claims again before the
    // next scan reads its slot.
    finish(&reader);
    claim_only(&mut reader);
    cell.publish(make(2)); // a new claim: floor 1, keeps 1, frees 0
    assert_eq!(freed(&sites), vec![0]);
    assert_eq!(cell.retired_len(), 1);

    finish(&reader);
    assert_eq!(cell.reclaim(), 0);
    assert_eq!(freed(&sites), vec![0, 1]);
}
