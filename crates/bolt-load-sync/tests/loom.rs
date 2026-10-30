//! Model checks of the `watch` and `slot` protocols.
//!
//! Run with `RUSTFLAGS="--cfg loom" cargo test -p bolt-load-sync --release --test loom`.
//! Each model stays small: loom explores every interleaving of it.
#![cfg(loom)]

use bolt_load_sync::{
    slot,
    watch::{self, Closed},
};
use futures::StreamExt;
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

/// `wait_for` racing a send is never lost: the sender stays alive until the
/// waiter returns, so a lost wake deadlocks the model.
#[test]
fn wait_for_catches_a_send_during_registration() {
    loom::model(|| {
        let (tx, mut rx) = watch::channel(0);
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            tx
        });
        assert_eq!(block_on(rx.wait_for(|v| *v == 1)).map(|v| *v), Ok(1));
        drop(sender.join().unwrap());
    });
}

/// `wait_for` tests the final value before reporting `Closed`, however the
/// last send and the close interleave with it.
#[test]
fn wait_for_tests_the_final_value_racing_close() {
    loom::model(|| {
        let (tx, mut rx) = watch::channel(0);
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            drop(tx);
        });
        assert_eq!(block_on(rx.wait_for(|v| *v == 1)).map(|v| *v), Ok(1));
        assert!(matches!(block_on(rx.wait_for(|v| *v == 2)), Err(Closed)));
        sender.join().unwrap();
    });
}

/// A pending stream registered while a send happens still yields it.
#[test]
fn stream_catches_a_send_during_registration() {
    loom::model(|| {
        let (tx, rx) = watch::channel(0);
        let mut stream = rx.into_stream();
        assert_eq!(block_on(stream.next()).map(|v| *v), Some(0));
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            tx
        });
        assert_eq!(block_on(stream.next()).map(|v| *v), Some(1));
        drop(sender.join().unwrap());
    });
}

/// The stream yields the final value before it ends.
#[test]
fn stream_yields_the_final_value_racing_close() {
    loom::model(|| {
        let (tx, rx) = watch::channel(0);
        let mut stream = rx.into_stream();
        assert_eq!(block_on(stream.next()).map(|v| *v), Some(0));
        let sender = thread::spawn(move || {
            tx.send_replace(1);
            drop(tx);
        });
        assert_eq!(block_on(stream.next()).map(|v| *v), Some(1));
        assert!(block_on(stream.next()).is_none());
        sender.join().unwrap();
    });
}

/// A send racing a waiting receiver is never lost: a lost wake deadlocks the
/// model, since the sender stays alive until the receiver returns.
#[test]
fn slot_send_wakes_the_receiver() {
    loom::model(|| {
        let (tx, mut rx) = slot::channel();
        let sender = thread::spawn(move || {
            assert!(tx.send(5));
            tx
        });
        assert_eq!(block_on(rx.recv()), Some(5));
        drop(sender.join().unwrap());
    });
}

/// Closing right after the last send still delivers that value first.
#[test]
fn slot_last_value_arrives_before_closed() {
    loom::model(|| {
        let (tx, mut rx) = slot::channel();
        let sender = thread::spawn(move || {
            assert!(tx.send(7));
            drop(tx);
        });
        assert_eq!(block_on(rx.recv()), Some(7));
        assert_eq!(block_on(rx.recv()), None);
        sender.join().unwrap();
    });
}

/// Dropping the sender wakes a waiting receiver.
#[test]
fn slot_close_wakes_the_receiver() {
    loom::model(|| {
        let (tx, mut rx) = slot::channel();
        let sender = thread::spawn(move || drop(tx));
        assert_eq!(block_on(rx.recv()), None);
        sender.join().unwrap();
    });
}

/// Racing two sends, the receiver reads each value at most once, in send
/// order, and always reads the last one.
#[test]
fn slot_receiver_reads_the_last_of_racing_sends() {
    loom::model(|| {
        let (tx, mut rx) = slot::channel();
        let sender = thread::spawn(move || {
            assert!(tx.send(2));
            assert!(tx.send(1));
            tx
        });
        let first = block_on(rx.recv()).unwrap();
        if first == 2 {
            assert_eq!(block_on(rx.recv()), Some(1));
        } else {
            assert_eq!(first, 1);
        }
        let tx = sender.join().unwrap();
        assert_eq!(rx.try_recv(), None);
        assert_eq!(tx.unread(), None);
    });
}

/// A receiver dropped on another thread makes later sends fail.
#[test]
fn slot_send_fails_once_the_receiver_is_gone() {
    loom::model(|| {
        let (tx, rx) = slot::channel();
        let receiver = thread::spawn(move || drop(rx));
        // Racing the drop, the send may still succeed.
        let _ = tx.send(1);
        receiver.join().unwrap();
        assert!(!tx.send(2));
    });
}

/// A receiver that re-registers between two sends still sees the second:
/// this reaches the `register` path that wakes on behalf of a racing wake.
#[test]
fn slot_reregistering_receiver_misses_no_send() {
    loom::model(|| {
        let (tx, mut rx) = slot::channel();
        let sender = thread::spawn(move || {
            assert!(tx.send(2));
            assert!(tx.send(1));
            tx
        });
        let mut last = u64::MAX;
        while last != 1 {
            let value = block_on(rx.recv()).unwrap();
            assert!(value < last, "{value} after {last}");
            last = value;
        }
        drop(sender.join().unwrap());
    });
}
