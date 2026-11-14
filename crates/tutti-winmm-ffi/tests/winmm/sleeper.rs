use std::time::{Duration, Instant};

use tutti_winmm_ffi::{Sleeper, Woken};

#[test]
fn a_wait_ends_at_its_time() {
    let sleeper = Sleeper::new().unwrap();
    let start = Instant::now();
    assert_eq!(
        sleeper.wait(Some(Duration::from_millis(30))).unwrap(),
        Woken::Elapsed
    );
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(25), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
}

#[test]
fn a_wake_made_before_the_wait_ends_it_at_once() {
    let sleeper = Sleeper::new().unwrap();
    sleeper.wake().unwrap();
    let start = Instant::now();
    assert_eq!(
        sleeper.wait(Some(Duration::from_secs(30))).unwrap(),
        Woken::Signalled
    );
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_wake_from_another_thread_ends_a_wait_with_no_time() {
    let sleeper = std::sync::Arc::new(Sleeper::new().unwrap());
    let waker = std::sync::Arc::clone(&sleeper);
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        waker.wake().unwrap();
    });
    assert_eq!(sleeper.wait(None).unwrap(), Woken::Signalled);
    thread.join().unwrap();
}
