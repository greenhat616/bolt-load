//! Fault injection primitives for test adapters.
//!
//! Built on `async-channel`, so they work under both Tokio and Smol.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use bolt_load_core::adapter::{
    AdapterError,
    error::{retryable::RetryableError, unretryable::InternalSnafu},
};

/// A one-shot barrier: callers of [`Gate::pass`] block until [`Gate::release`].
///
/// Every arrival is announced, so tests can wait for [`Gate::entered`]
/// instead of sleeping.
#[derive(Clone, Debug)]
pub struct Gate {
    entered_tx: async_channel::Sender<()>,
    entered_rx: async_channel::Receiver<()>,
    release_tx: async_channel::Sender<()>,
    release_rx: async_channel::Receiver<()>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        let (entered_tx, entered_rx) = async_channel::unbounded();
        let (release_tx, release_rx) = async_channel::bounded(1);
        Self {
            entered_tx,
            entered_rx,
            release_tx,
            release_rx,
        }
    }

    /// Announce the arrival, then wait until the gate is released.
    pub async fn pass(&self) {
        let _ = self.entered_tx.try_send(());
        // Nothing is ever sent; `recv` returns once the channel is closed.
        let _ = self.release_rx.recv().await;
    }

    /// Wait until one more caller has arrived at the gate.
    pub async fn entered(&self) {
        let _ = self.entered_rx.recv().await;
    }

    /// Open the gate for all current and future callers.
    pub fn release(&self) {
        self.release_tx.close();
    }
}

/// The kind of an injected failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    Retryable,
    Unretryable,
}

impl FaultKind {
    pub fn error(self, context: &str) -> AdapterError {
        match self {
            FaultKind::Retryable => RetryableError::Io {
                source: Arc::new(std::io::Error::other(format!(
                    "injected retryable fault: {context}"
                ))),
            }
            .into(),
            FaultKind::Unretryable => InternalSnafu {
                message: format!("injected unretryable fault: {context}"),
            }
            .build()
            .into(),
        }
    }
}

/// How connection attempts behave.
#[derive(Clone, Debug, Default)]
pub enum FaultPlan {
    /// Connect normally.
    #[default]
    None,
    /// Fail the first `n` attempts, then connect normally.
    FailFirst(usize, FaultKind),
    /// Fail every attempt.
    AlwaysFail(FaultKind),
    /// Block every attempt at the gate until it is released.
    Block(Gate),
    /// Never complete the `n`-th (0-based) attempt; connect the others normally.
    HangAt(usize),
}

impl FaultPlan {
    /// Apply the plan to the `attempt`-th (0-based) connection attempt.
    pub async fn apply(&self, attempt: usize, context: &str) -> Result<(), AdapterError> {
        match self {
            FaultPlan::None => Ok(()),
            FaultPlan::FailFirst(n, kind) if attempt < *n => Err(kind.error(context)),
            FaultPlan::FailFirst(..) => Ok(()),
            FaultPlan::AlwaysFail(kind) => Err(kind.error(context)),
            FaultPlan::Block(gate) => {
                gate.pass().await;
                Ok(())
            }
            FaultPlan::HangAt(n) if attempt == *n => std::future::pending().await,
            FaultPlan::HangAt(_) => Ok(()),
        }
    }
}

/// How the range fault server (`http_server::create_range_fault_server`)
/// answers. HEAD requests and requests without `Range` always get the whole
/// resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeFault {
    /// Answer range requests with 200 and the whole resource.
    IgnoreRange,
    /// Answer from one byte past the requested start, labelled honestly,
    /// except to the `bytes=0-0` probe. A one-byte range at the end starts
    /// one byte early instead.
    ShiftedStart,
    /// Name a total one byte too large, except to the `bytes=0-0` probe,
    /// as if the resource changed after the probe.
    WrongTotal,
    /// Answer range requests with 416 and `bytes */total`.
    Unsatisfiable,
    /// Answer range requests with 503 and `Retry-After`
    /// ([`CONGESTED_RETRY_AFTER_SECS`]).
    Congested,
    /// Answer the second range request other than the probe with 503 and
    /// `Retry-After` ([`CONGESTED_RETRY_AFTER_SECS`]). Send the other range
    /// responses at [`THROTTLED_BYTES_PER_SEC`], so that a task splits its
    /// first range while that one is still streaming.
    CongestedOnce,
    /// Mark range responses `Content-Encoding: gzip`; the body stays plain.
    Gzip,
    /// Serve the gzip-encoded resource to every request, despite
    /// `Accept-Encoding: identity`. Lengths and ranges count encoded bytes,
    /// as RFC 9110 has it.
    EncodedRepresentation,
    /// Cut the first range response other than the probe off halfway
    /// through its body.
    Truncated,
    /// Answer the first range request other than the probe with the first
    /// half of the requested range. From then on, answer every request with
    /// 200 and [`RangeFault::replacement`], as if the resource were replaced
    /// by a server that ignores ranges.
    ReplacedAfterShortRange,
    /// Answer the first range request that starts past the start of the
    /// resource with the first half of its body, then send nothing more,
    /// without closing the connection. A task only gets past it within its
    /// idle timeout by requesting the rest again, so it is not in
    /// [`RangeFault::ALL`].
    StalledOnce,
    /// Serve the resource with validators (see [`RangeFault::VALIDATED`]):
    /// `ETag: "v1"`, and a `Last-Modified` long before `Date`. Answer the
    /// first range request other than the probe with the first half of
    /// its range; from then on, serve [`RangeFault::replacement`] as `"v2"`.
    /// A range request whose `If-Range` names another version gets 200 and
    /// the whole current version.
    ///
    /// Every response of these faults sends `Date`, `ETag` and
    /// `Last-Modified` from one snapshot: `Date` is
    /// [`VALIDATED_DATE`], and `Last-Modified` long before it unless the
    /// fault says otherwise. `If-Range` is evaluated as RFC 9110 §13.1.5
    /// says, with dates compared as dates.
    ChangedMidDownload,
    /// [`RangeFault::ChangedMidDownload`], but once replaced, the server
    /// ignores ranges: it answers every request with 200 and the whole
    /// replacement, `"v2"`.
    ChangedThenIgnoresRange,
    /// Answer range requests with 200 and the whole resource, with the
    /// validators of `"v1"`, whatever `If-Range` names.
    IgnoreRangeWithValidators,
    /// A weak `ETag: W/"w1"`, and a `Last-Modified` long before `Date`;
    /// `If-Range` is evaluated against that date.
    WeakEtag,
    /// No `ETag`, and a `Last-Modified` equal to `Date`, so no strong
    /// validator.
    RecentLastModified,
    /// No `ETag`, and a `Last-Modified` exactly one second before `Date`.
    LastModifiedOneSecondOld,
    /// No `ETag`, and a `Last-Modified` one second after `Date`.
    FutureLastModified,
    /// No `ETag`; `Date` in the asctime format, and `Last-Modified` in the
    /// RFC 850 format, except on a range response other than the probe,
    /// which sends the same date as an IMF-fixdate.
    ObsoleteDateFormats,
    /// [`RangeFault::ChangedMidDownload`] with [`RangeFault::changed`],
    /// which has the same size, but the server ignores `If-Range`: it
    /// answers every range request with 206 and the current version, with
    /// its `ETag`.
    ChangedIgnoringIfRange,
    /// [`RangeFault::ChangedIgnoringIfRange`] without an `ETag`: the
    /// versions differ in their `Last-Modified`.
    ChangedByDateIgnoringIfRange,
    /// The validators of [`RangeFault::ChangedMidDownload`], but range
    /// responses other than the probe carry neither `ETag` nor
    /// `Last-Modified`; the resource does not change.
    PartsWithoutValidators,
    /// A HEAD gets `ETag: "h1"`, a GET `ETag: "v1"`; the resource does not
    /// change.
    HeadValidatorsDiffer,
    /// [`RangeFault::HeadValidatorsDiffer`], changed as
    /// [`RangeFault::ChangedMidDownload`] is: a HEAD then gets `"h2"`.
    HeadValidatorsDifferThenChanged,
    /// [`RangeFault::IgnoreRangeWithValidators`], but a HEAD gets
    /// `ETag: "h1"`.
    IgnoreRangeWithHeadValidators,
    /// Two `ETag` field lines, `"v1"` and `"x1"`, so no entity tag, and a
    /// `Last-Modified` long before `Date`.
    RepeatedEtag,
    /// One `ETag` field line holding a list, `"v1", "x1"`, so no entity
    /// tag, and a `Last-Modified` long before `Date`.
    ListEtag,
    /// The validators of [`RangeFault::ChangedMidDownload`], but the
    /// `bytes=0-0` probe gets 403; other range requests are answered. The
    /// resource does not change.
    RejectedProbe,
    /// A HEAD names a size 16 bytes larger than the resource; a GET, and
    /// the `Content-Range` of its 206, the real one. The resource does not
    /// change.
    HeadSizeDiffers,
    /// The validators of [`RangeFault::ChangedMidDownload`], `"v1"`, and
    /// ranges answered as asked; the resource never changes. Served by
    /// [`create_range_fault_server_held_at`](super::http_server::create_range_fault_server_held_at),
    /// a range that covers the position held holds its body there. Not in
    /// [`RangeFault::VALIDATED`], whose servers change.
    Stable,
}

/// The `Date` of every response of the faults of
/// [`RangeFault::VALIDATED`]: 2027-01-15T08:00:00Z.
pub const VALIDATED_DATE: std::time::Duration = std::time::Duration::from_secs(1_800_000_000);

/// The `Retry-After` seconds of [`RangeFault::Congested`] and
/// [`RangeFault::CongestedOnce`].
pub const CONGESTED_RETRY_AFTER_SECS: u64 = 2;

/// The rate of the range responses of [`RangeFault::CongestedOnce`].
pub const THROTTLED_BYTES_PER_SEC: u64 = 4 << 20;

impl RangeFault {
    pub const ALL: [Self; 10] = [
        Self::IgnoreRange,
        Self::ShiftedStart,
        Self::WrongTotal,
        Self::Unsatisfiable,
        Self::Congested,
        Self::CongestedOnce,
        Self::Gzip,
        Self::EncodedRepresentation,
        Self::Truncated,
        Self::ReplacedAfterShortRange,
    ];

    /// The faults that send validators and evaluate `If-Range`. Not in
    /// [`RangeFault::ALL`]: they answer a conditional range request, which
    /// the faults there ignore. A server holds one version for all of them,
    /// so a test uses one of them per server.
    pub const VALIDATED: [Self; 18] = [
        Self::ChangedMidDownload,
        Self::ChangedThenIgnoresRange,
        Self::IgnoreRangeWithValidators,
        Self::WeakEtag,
        Self::RecentLastModified,
        Self::LastModifiedOneSecondOld,
        Self::FutureLastModified,
        Self::ObsoleteDateFormats,
        Self::ChangedIgnoringIfRange,
        Self::ChangedByDateIgnoringIfRange,
        Self::PartsWithoutValidators,
        Self::HeadValidatorsDiffer,
        Self::HeadValidatorsDifferThenChanged,
        Self::IgnoreRangeWithHeadValidators,
        Self::RepeatedEtag,
        Self::ListEtag,
        Self::RejectedProbe,
        Self::HeadSizeDiffers,
    ];

    /// The URL path that serves this fault.
    pub fn path(self) -> &'static str {
        match self {
            Self::IgnoreRange => "/ignore-range",
            Self::ShiftedStart => "/shifted-start",
            Self::WrongTotal => "/wrong-total",
            Self::Unsatisfiable => "/unsatisfiable",
            Self::Congested => "/congested",
            Self::CongestedOnce => "/congested-once",
            Self::Gzip => "/gzip",
            Self::EncodedRepresentation => "/encoded-representation",
            Self::Truncated => "/truncated",
            Self::ReplacedAfterShortRange => "/replaced-after-short-range",
            Self::StalledOnce => "/stalled-once",
            Self::ChangedMidDownload => "/changed-mid-download",
            Self::ChangedThenIgnoresRange => "/changed-then-ignores-range",
            Self::IgnoreRangeWithValidators => "/ignore-range-with-validators",
            Self::WeakEtag => "/weak-etag",
            Self::RecentLastModified => "/recent-last-modified",
            Self::LastModifiedOneSecondOld => "/last-modified-one-second-old",
            Self::FutureLastModified => "/future-last-modified",
            Self::ObsoleteDateFormats => "/obsolete-date-formats",
            Self::ChangedIgnoringIfRange => "/changed-ignoring-if-range",
            Self::ChangedByDateIgnoringIfRange => "/changed-by-date-ignoring-if-range",
            Self::PartsWithoutValidators => "/parts-without-validators",
            Self::HeadValidatorsDiffer => "/head-validators-differ",
            Self::HeadValidatorsDifferThenChanged => "/head-validators-differ-then-changed",
            Self::IgnoreRangeWithHeadValidators => "/ignore-range-with-head-validators",
            Self::RepeatedEtag => "/repeated-etag",
            Self::ListEtag => "/list-etag",
            Self::RejectedProbe => "/rejected-probe",
            Self::HeadSizeDiffers => "/head-size-differs",
            Self::Stable => "/stable",
        }
    }

    /// The second version of [`RangeFault::ChangedIgnoringIfRange`] and the
    /// faults like it: every byte of `content` plus one, so it has the same
    /// size and differs everywhere.
    pub fn changed(content: &[u8]) -> bytes::Bytes {
        content.iter().map(|byte| byte.wrapping_add(1)).collect()
    }

    /// What [`RangeFault::ReplacedAfterShortRange`] serves once replaced:
    /// the last sixteenth of `content`, at least one byte. It differs, and is
    /// shorter than what the first ranges write, so a file that was not
    /// truncated shows their bytes after it.
    pub fn replacement(content: &[u8]) -> bytes::Bytes {
        let len = (content.len() / 16).max(1);
        bytes::Bytes::copy_from_slice(&content[content.len() - len..])
    }

    /// The fault served at `/{name}`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .chain([Self::StalledOnce, Self::Stable])
            .chain(Self::VALIDATED)
            .find(|fault| fault.path().strip_prefix('/') == Some(name))
    }
}

/// How [`create_limited_range_server`](super::http_server::create_limited_range_server)
/// turns away a connection past its limit, as a server with a silent
/// connection limit does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitMode {
    /// Close the connection as soon as it is accepted, before reading the
    /// request: the client sees the connection closed or reset.
    Close,
    /// Accept the connection, then never read or answer the request: the
    /// client waits until its own timeout.
    Hang,
}

/// Increments a counter for as long as it is alive.
#[derive(Debug)]
pub struct CounterGuard(Arc<AtomicUsize>);

impl CounterGuard {
    pub fn new(counter: &Arc<AtomicUsize>) -> Self {
        Self::counted(counter).0
    }

    /// [`new`](Self::new), also returning the count this guard's own
    /// increment made, whatever other guards do afterwards.
    pub fn counted(counter: &Arc<AtomicUsize>) -> (Self, usize) {
        let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
        (Self(counter.clone()), count)
    }
}

impl Drop for CounterGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn test_gate_blocks_until_released() {
        let gate = Gate::new();
        let waiter = tokio::spawn({
            let gate = gate.clone();
            async move { gate.pass().await }
        });
        gate.entered().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        gate.release();
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        // Released gates let later callers through immediately.
        tokio::time::timeout(Duration::from_secs(1), gate.pass())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_fault_plan() {
        let plan = FaultPlan::FailFirst(2, FaultKind::Retryable);
        assert!(plan.apply(0, "t").await.is_err());
        assert!(plan.apply(1, "t").await.is_err());
        assert!(plan.apply(2, "t").await.is_ok());
        assert!(matches!(
            FaultPlan::AlwaysFail(FaultKind::Unretryable)
                .apply(5, "t")
                .await,
            Err(AdapterError::Unretryable { .. })
        ));
    }

    #[test]
    fn test_counter_guard() {
        let counter = Arc::new(AtomicUsize::new(0));
        let a = CounterGuard::new(&counter);
        let b = CounterGuard::new(&counter);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        drop(a);
        drop(b);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    /// The count is the one the increment made, not a later read: a guard
    /// dropped in between does not lower it.
    #[test]
    fn test_counter_guard_counts_its_own_increment() {
        let counter = Arc::new(AtomicUsize::new(0));
        let a = CounterGuard::new(&counter);
        let (b, count) = CounterGuard::counted(&counter);
        drop(a);
        assert_eq!(count, 2);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        drop(b);
        let (_c, count) = CounterGuard::counted(&counter);
        assert_eq!(count, 1);
    }
}
