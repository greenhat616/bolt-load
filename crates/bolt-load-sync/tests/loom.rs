//! Model checks of the `watch` protocol.
//!
//! Run with `RUSTFLAGS="--cfg loom" cargo test -p bolt-load-sync --release --test loom`.
//! Each model stays small: loom explores every interleaving of it.
#![cfg(loom)]

use bolt_load_sync::watch::{self, Closed};
use loom::{future::block_on, thread};

/// A send racing an async waiter is never lost.
#[test]
fn send_wakes_an_async_waiter() {
    loom::model(|| {
        let (tx, mut rx) = watch::channel(0);
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            tx
        });
        block_on(rx.changed()).unwrap();
        assert_eq!(*rx.borrow_and_update(), 1);
        drop(sender.join().unwrap());
    });
}

/// A send racing a blocking waiter is never lost.
#[test]
fn send_wakes_a_blocking_waiter() {
    loom::model(|| {
        let (tx, mut rx) = watch::channel(0);
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            tx
        });
        rx.changed_blocking().unwrap();
        assert_eq!(*rx.borrow_and_update(), 1);
        drop(sender.join().unwrap());
    });
}

/// Closing right after the last send still delivers that value first.
#[test]
fn the_last_value_arrives_before_closed() {
    loom::model(|| {
        let (tx, mut rx) = watch::channel(0);
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            drop(tx);
        });
        // The close is published after the send, so the value comes first.
        block_on(rx.changed()).unwrap();
        assert_eq!(*rx.borrow_and_update(), 1);
        assert_eq!(block_on(rx.changed()), Err(Closed));
        sender.join().unwrap();
    });
}

/// `borrow_and_update` records the version of the value it returns: a later
/// `has_changed` is true exactly when a newer value exists.
#[test]
fn borrow_and_update_matches_its_version() {
    loom::model(|| {
        let (tx, mut rx) = watch::channel(0);
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            tx.send_replace(2);
            tx
        });
        let seen = *rx.borrow_and_update();
        let tx = sender.join().unwrap();
        assert_eq!(rx.has_changed(), seen != 2);
        drop(tx);
    });
}

/// One send wakes every waiting receiver: both may be registered before the
/// send, so waking only one of them deadlocks the model.
///
/// Three threads make the full search impractical; two preemptions still reach
/// both receivers parked before the send (`notify(1)` fails within this bound).
#[test]
fn a_send_wakes_every_receiver() {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(2);
    model.check(|| {
        let (tx, rx) = watch::channel(0);
        let waiters: Vec<_> = [rx.clone(), rx]
            .into_iter()
            .map(|mut rx| {
                thread::spawn(move || {
                    block_on(rx.changed()).unwrap();
                    *rx.borrow_and_update()
                })
            })
            .collect();
        tx.send_replace(1);
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap(), 1);
        }
        // Kept alive until here, so a close cannot stand in for a lost wakeup.
        drop(tx);
    });
}
