use std::{
    io::{BufWriter, Seek, Write},
    ops::Bound,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime},
};

use axum::{
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use rand::RngExt;
use tempfile::tempfile;
use tokio::net::TcpListener;

use super::fault::{
    CONGESTED_RETRY_AFTER_SECS, Gate, LimitMode, RangeFault, THROTTLED_BYTES_PER_SEC,
    VALIDATED_DATE,
};

/// Create a random file with given size, useful for testing
pub fn create_random_file(size: usize) -> std::io::Result<std::fs::File> {
    let mut file = tempfile()?;
    let mut writer = BufWriter::new(file.try_clone()?);
    let mut rng = rand::rng();
    let mut buffer = [0; 1024];
    let mut remaining_size = size;
    while remaining_size > 0 {
        let bytes_to_write = std::cmp::min(remaining_size, buffer.len());
        rng.fill(&mut buffer[..bytes_to_write]);
        writer.write_all(&buffer[..bytes_to_write])?;
        remaining_size -= bytes_to_write;
    }
    // ensure buffered data hits disk before we read/serve it
    writer.flush()?;
    file.sync_all()?;
    // reset the file pointer to the beginning
    file.seek(std::io::SeekFrom::Start(0))?;
    Ok(file)
}

#[derive(Clone)]
struct FileHolder(Arc<tokio::sync::Mutex<tokio::fs::File>>);

pub const DEFAULT_FILE_SIZE: usize = 1024 * 1024;

/// The `Retry-After` seconds that `/status/429` and `/status/503` send.
pub const STATUS_RETRY_AFTER_SECS: u64 = 7;

pub async fn create_http_server() -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    create_http_server_with_file_size(DEFAULT_FILE_SIZE).await
}

pub async fn create_http_server_with_file_size(
    file_size: usize,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    use axum::{extract::State, response::IntoResponse};
    use axum_extra::TypedHeader;
    use tokio::io::AsyncSeekExt;

    let file = tokio::task::spawn_blocking(move || create_random_file(file_size)).await??;
    let holder = FileHolder(Arc::new(tokio::sync::Mutex::new(
        tokio::fs::File::from_std(file),
    )));
    let port =
        portpicker::pick_unused_port().ok_or(anyhow::anyhow!("Failed to pick an unused port"))?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;

    /// a handler send without range
    async fn no_range_handler(
        State(holder): State<FileHolder>,
    ) -> impl axum::response::IntoResponse {
        let mut file = holder.0.lock().await;
        match file.seek(std::io::SeekFrom::Start(0)).await {
            Ok(_) => {
                let file_size = file.metadata().await.unwrap().len();
                let reader = tokio_util::io::ReaderStream::new(file.try_clone().await.unwrap());
                let body = axum::body::Body::from_stream(reader);
                let headers = [
                    (
                        axum::http::header::CONTENT_TYPE,
                        "text/plain; charset=utf-8",
                    ),
                    (axum::http::header::CONTENT_LENGTH, &format!("{file_size}")),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        "attachment; filename=\"test.txt\"",
                    ),
                ];
                (headers, body).into_response()
            }
            Err(e) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string().into_response(),
            )
                .into_response(),
        }
    }

    /// a handler mock range stream
    async fn range_handler(
        State(holder): State<FileHolder>,
        range: Option<TypedHeader<axum_extra::headers::Range>>,
    ) -> impl axum::response::IntoResponse {
        let file = holder.0.lock().await;
        let mut file_cloned = file.try_clone().await.unwrap();
        file_cloned.seek(std::io::SeekFrom::Start(0)).await.unwrap();
        let body = axum_range::KnownSize::file(file_cloned).await.unwrap();
        let range = range.map(|TypedHeader(range)| range);
        let ranged = axum_range::Ranged::new(range, body);
        ranged.into_response()
    }

    /// a handler answering with the status in the path
    async fn status_handler(
        axum::extract::Path(code): axum::extract::Path<u16>,
    ) -> axum::response::Response {
        let Ok(status) = axum::http::StatusCode::from_u16(code) else {
            return axum::http::StatusCode::BAD_REQUEST.into_response();
        };
        let mut response = status.into_response();
        if matches!(code, 429 | 503) {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(STATUS_RETRY_AFTER_SECS),
            );
        }
        response
    }

    let app = axum::Router::new()
        .route("/no_range", axum::routing::get(no_range_handler))
        .route("/range", axum::routing::get(range_handler))
        .route("/status/{code}", axum::routing::get(status_handler))
        .with_state(holder);

    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap()
    });

    Ok((port, handle))
}

#[derive(Clone)]
struct FaultState {
    content: Bytes,
    /// `content` as gzip, for [`RangeFault::EncodedRepresentation`]; encoded
    /// on first use, which large contents rarely need.
    encoded: Arc<OnceLock<Bytes>>,
    /// Whether [`RangeFault::Truncated`] already cut a response.
    truncated: Arc<AtomicBool>,
    /// Whether [`RangeFault::ReplacedAfterShortRange`] serves the
    /// replacement.
    replaced: Arc<AtomicBool>,
    /// The range requests other than the probe that
    /// [`RangeFault::CongestedOnce`] answered.
    congested_once: Arc<AtomicUsize>,
    /// Where the first range response of [`RangeFault::CongestedOnce`],
    /// and the one [`RangeFault::StalledOnce`] stalls, hold their body.
    hold: Option<Gate>,
    /// Whether [`RangeFault::StalledOnce`] already stalled a response.
    stalled: Arc<AtomicBool>,
    /// The version the faults of [`RangeFault::VALIDATED`] serve, from 1.
    version: Arc<AtomicUsize>,
    log: RequestLog,
}

/// A request the range fault server answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedRequest {
    /// When the request arrived.
    pub at: Instant,
    pub method: Method,
    pub fault: Option<RangeFault>,
    /// The requested range as `(first, last)`; `None` without `Range`.
    pub range: Option<(u64, u64)>,
    /// The `If-Range` the request sent.
    pub if_range: Option<String>,
    pub status: StatusCode,
}

/// The requests a range fault server answered, in the order it answered
/// them.
#[derive(Clone, Debug, Default)]
pub struct RequestLog(Arc<Mutex<Vec<RecordedRequest>>>);

impl RequestLog {
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.0.lock().unwrap().clone()
    }

    fn record(&self, request: RecordedRequest) {
        self.0.lock().unwrap().push(request);
    }
}

/// Serve `content` at every [`RangeFault::path`], misbehaving on range
/// requests as the fault says. HEAD and `GET` without `Range` answer
/// normally, with `Accept-Ranges: bytes` so that clients try ranges.
pub async fn create_range_fault_server(
    content: Bytes,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>)> {
    let (port, handle, _log) = create_range_fault_server_with_log(content, None).await?;
    Ok((port, handle))
}

/// [`create_range_fault_server`], also returning the log of the requests
/// it answers.
///
/// With `hold`, the first range response of [`RangeFault::CongestedOnce`]
/// other than the probe, and the response [`RangeFault::StalledOnce`]
/// stalls, send their headers, then hold their body at the gate until the
/// gate is released.
pub async fn create_range_fault_server_with_log(
    content: Bytes,
    hold: Option<Gate>,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>, RequestLog)> {
    let port =
        portpicker::pick_unused_port().ok_or(anyhow::anyhow!("Failed to pick an unused port"))?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    let log = RequestLog::default();
    let state = FaultState {
        encoded: Arc::default(),
        content,
        truncated: Arc::new(AtomicBool::new(false)),
        replaced: Arc::new(AtomicBool::new(false)),
        stalled: Arc::new(AtomicBool::new(false)),
        congested_once: Arc::new(AtomicUsize::new(0)),
        version: Arc::new(AtomicUsize::new(1)),
        hold,
        log: log.clone(),
    };
    let app = axum::Router::new()
        .route("/{fault}", axum::routing::get(fault_handler))
        .with_state(state);
    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap()
    });
    Ok((port, handle, log))
}

/// `content` as a gzip stream.
pub fn gzip(content: &[u8]) -> std::io::Result<Bytes> {
    use std::io::Write;

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(content)?;
    Ok(encoder.finish()?.into())
}

/// The first range of a `Range: bytes=…` header, as `(first, last)`.
fn requested_range(headers: &HeaderMap, len: u64) -> Option<(u64, u64)> {
    use axum_extra::headers::{Header, Range};

    let range = Range::decode(&mut headers.get_all(header::RANGE).iter()).ok()?;
    let (start, end) = range.satisfiable_ranges(len).next()?;
    let first = match start {
        Bound::Included(first) => first,
        Bound::Excluded(first) => first + 1,
        Bound::Unbounded => 0,
    };
    let last = match end {
        Bound::Included(last) => last.min(len - 1),
        Bound::Excluded(end) => end.min(len) - 1,
        Bound::Unbounded => len - 1,
    };
    (first <= last).then_some((first, last))
}

fn partial_headers(first: u64, last: u64, total: u64) -> [(header::HeaderName, String); 2] {
    [
        (
            header::CONTENT_RANGE,
            format!("bytes {first}-{last}/{total}"),
        ),
        (header::CONTENT_LENGTH, (last - first + 1).to_string()),
    ]
}

fn partial(content: &Bytes, first: u64, last: u64, total: u64) -> Response {
    let body = content.slice(first as usize..=last as usize);
    (
        StatusCode::PARTIAL_CONTENT,
        partial_headers(first, last, total),
        body,
    )
        .into_response()
}

/// A 206 for `first..=last` whose body fails halfway, which aborts the
/// connection before `Content-Length` is reached.
fn truncated(content: &Bytes, first: u64, last: u64, total: u64) -> Response {
    let body = content.slice(first as usize..=last as usize);
    let stream = async_stream::stream! {
        yield Ok(body.slice(..body.len() / 2));
        // Pending once, so the server flushes the headers and the first half
        // before the failure drops them.
        tokio::task::yield_now().await;
        yield Err(std::io::Error::other("truncated by the fault server"));
    };
    (
        StatusCode::PARTIAL_CONTENT,
        partial_headers(first, last, total),
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

/// A 206 for `first..=last` that sends the first half of its body, then
/// nothing more, without closing. With `hold`, the body waits at the gate
/// first; the headers are sent before.
fn stalled(content: &Bytes, first: u64, last: u64, total: u64, hold: Option<Gate>) -> Response {
    let body = content.slice(first as usize..=last as usize);
    let stream = async_stream::stream! {
        if let Some(gate) = hold {
            gate.pass().await;
        }
        yield Ok::<_, std::io::Error>(body.slice(..body.len() / 2));
        std::future::pending::<()>().await;
    };
    (
        StatusCode::PARTIAL_CONTENT,
        partial_headers(first, last, total),
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

/// A 206 for `first..=last`, sent at [`THROTTLED_BYTES_PER_SEC`]. With
/// `hold`, the body waits at the gate; the headers are sent before.
fn throttled(content: &Bytes, first: u64, last: u64, total: u64, hold: Option<Gate>) -> Response {
    paced(content, first, last, total, hold, THROTTLED_BYTES_PER_SEC)
}

/// A 206 for `first..=last`, sent at `bytes_per_sec`. With `hold`, the
/// body waits at the gate; the headers are sent before.
fn paced(
    content: &Bytes,
    first: u64,
    last: u64,
    total: u64,
    hold: Option<Gate>,
    bytes_per_sec: u64,
) -> Response {
    const FRAME: usize = 64 * 1024;
    let body = content.slice(first as usize..=last as usize);
    let stream = async_stream::stream! {
        if let Some(gate) = hold {
            gate.pass().await;
        }
        let start = tokio::time::Instant::now();
        let mut sent = 0;
        while sent < body.len() {
            // Paced from the start, so coarse timers keep the average rate.
            let due = Duration::from_secs_f64(sent as f64 / bytes_per_sec as f64);
            tokio::time::sleep_until(start + due).await;
            let end = (sent + FRAME).min(body.len());
            yield Ok::<_, std::io::Error>(body.slice(sent..end));
            sent = end;
        }
    };
    (
        StatusCode::PARTIAL_CONTENT,
        partial_headers(first, last, total),
        axum::body::Body::from_stream(stream),
    )
        .into_response()
}

fn congested() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, CONGESTED_RETRY_AFTER_SECS.to_string())],
    )
        .into_response()
}

/// What a response of a fault of [`RangeFault::VALIDATED`] says about the
/// version it serves, from one snapshot: its `Date`, and its validators.
struct Snapshot {
    date: SystemTime,
    /// The `ETag` that `If-Range` is evaluated against, if any.
    etag: Option<String>,
    last_modified: SystemTime,
}

impl Snapshot {
    fn of(fault: RangeFault, version: usize, head: bool) -> Self {
        use RangeFault::*;

        let date = SystemTime::UNIX_EPOCH + VALIDATED_DATE;
        // Long before `Date`, a second apart per version.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + version as u64);
        let second = Duration::from_secs(1);
        let strong = Some(format!("\"v{version}\""));
        let (etag, last_modified) = match fault {
            WeakEtag => (Some(format!("W/\"w{version}\"")), old),
            RecentLastModified => (None, date),
            LastModifiedOneSecondOld => (None, date - second),
            FutureLastModified => (None, date + second),
            ObsoleteDateFormats | ChangedByDateIgnoringIfRange | RepeatedEtag | ListEtag => {
                (None, old)
            }
            HeadValidatorsDiffer
            | HeadValidatorsDifferThenChanged
            | IgnoreRangeWithHeadValidators
                if head =>
            {
                (Some(format!("\"h{version}\"")), old)
            }
            _ => (strong, old),
        };
        Self {
            date,
            etag,
            last_modified,
        }
    }

    /// Whether `if_range` names this version, as RFC 9110 §13.1.5
    /// evaluates it: an entity tag strongly equal to its own, or a date
    /// equal to its `Last-Modified`, compared as dates.
    fn holds(&self, if_range: &str) -> bool {
        if if_range.starts_with('"') || if_range.starts_with("W/") {
            self.etag
                .as_deref()
                .is_some_and(|etag| !etag.starts_with("W/") && etag == if_range)
        } else {
            parse_http_date(if_range) == Some(self.last_modified)
        }
    }
}

/// An HTTP-date in any of its three formats (RFC 9110 §5.6.7).
fn parse_http_date(value: &str) -> Option<SystemTime> {
    use axum_extra::headers::{Header, LastModified};

    let value = HeaderValue::from_str(value).ok()?;
    LastModified::decode(&mut std::iter::once(&value))
        .ok()
        .map(SystemTime::from)
}

/// The formats of an HTTP-date.
#[derive(Clone, Copy)]
enum DateFormat {
    /// `Sun, 06 Nov 1994 08:49:37 GMT`
    Imf,
    /// `Sunday, 06-Nov-94 08:49:37 GMT`
    Rfc850,
    /// `Sun Nov  6 08:49:37 1994`
    Asctime,
}

fn http_date(time: SystemTime, format: DateFormat) -> String {
    use axum_extra::headers::{Header, LastModified};

    let mut values = Vec::new();
    LastModified::from(time).encode(&mut values);
    let imf = values[0].to_str().unwrap().to_owned();
    // `Sun, 06 Nov 1994 08:49:37 GMT`
    let [weekday, day, month, year, clock, _gmt] = imf
        .split(' ')
        .collect::<Vec<_>>()
        .try_into()
        .expect("an IMF-fixdate");
    let weekday = weekday.trim_end_matches(',');
    match format {
        DateFormat::Imf => imf,
        DateFormat::Rfc850 => {
            let long = [
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday",
            ]
            .into_iter()
            .find(|long| long.starts_with(weekday))
            .unwrap();
            format!("{long}, {day}-{month}-{} {clock} GMT", &year[2..])
        }
        DateFormat::Asctime => {
            let day = day.trim_start_matches('0');
            format!("{weekday} {month} {day:>2} {clock} {year}")
        }
    }
}

/// A response of a fault of [`RangeFault::VALIDATED`], with its validators.
fn validated_response(
    fault: RangeFault,
    state: &FaultState,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    use RangeFault::*;

    let head = method == Method::HEAD;
    let ranged = !head && headers.contains_key(header::RANGE);
    let version = state.version.load(Ordering::SeqCst);
    let content_of = |version: usize| match fault {
        ChangedMidDownload | ChangedThenIgnoresRange | HeadValidatorsDifferThenChanged
            if version > 1 =>
        {
            RangeFault::replacement(&state.content)
        }
        ChangedIgnoringIfRange | ChangedByDateIgnoringIfRange if version > 1 => {
            RangeFault::changed(&state.content)
        }
        _ => state.content.clone(),
    };
    let probe = requested_range(headers, content_of(version).len() as u64) == Some((0, 0));
    let changes = matches!(
        fault,
        ChangedMidDownload
            | ChangedThenIgnoresRange
            | ChangedIgnoringIfRange
            | ChangedByDateIgnoringIfRange
            | HeadValidatorsDifferThenChanged
    );
    // Only the request that changes it gets a half.
    let half = ranged
        && !probe
        && changes
        && state
            .version
            .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
    let content = content_of(version);
    let len = content.len() as u64;
    let snapshot = Snapshot::of(fault, version, head);
    let if_range = headers
        .get(header::IF_RANGE)
        .and_then(|value| value.to_str().ok());
    let ignores_range = fault == IgnoreRangeWithValidators
        || fault == IgnoreRangeWithHeadValidators
        || (fault == ChangedThenIgnoresRange && version > 1 && !half);
    let ignores_if_range = matches!(fault, ChangedIgnoringIfRange | ChangedByDateIgnoringIfRange);
    let full = !ranged
        || ignores_range
        || (!ignores_if_range && if_range.is_some_and(|value| !snapshot.holds(value)));
    let mut response = if fault == RejectedProbe && ranged && probe {
        StatusCode::FORBIDDEN.into_response()
    } else if fault == HeadSizeDiffers && head {
        let mut longer = content.to_vec();
        longer.extend_from_slice(&[0; 16]);
        Bytes::from(longer).into_response()
    } else if full {
        content.into_response()
    } else {
        match requested_range(headers, len) {
            Some((first, last)) if half => {
                partial(&content, first, first + (last - first) / 2, len)
            }
            Some((first, last)) => partial(&content, first, last, len),
            None => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response(),
        }
    };
    let part = response.status() == StatusCode::PARTIAL_CONTENT && !probe;
    let date_format = match fault {
        ObsoleteDateFormats if !part => DateFormat::Rfc850,
        _ => DateFormat::Imf,
    };
    let response_headers = response.headers_mut();
    let value = |value: String| HeaderValue::from_str(&value).unwrap();
    if !(fault == PartsWithoutValidators && part) {
        match fault {
            RepeatedEtag => {
                response_headers.append(header::ETAG, value(format!("\"v{version}\"")));
                response_headers.append(header::ETAG, value(format!("\"x{version}\"")));
            }
            ListEtag => {
                response_headers.insert(
                    header::ETAG,
                    value(format!("\"v{version}\", \"x{version}\"")),
                );
            }
            _ => {
                if let Some(etag) = snapshot.etag {
                    response_headers.insert(header::ETAG, value(etag));
                }
            }
        }
        response_headers.insert(
            header::LAST_MODIFIED,
            value(http_date(snapshot.last_modified, date_format)),
        );
    }
    let date_format = match fault {
        ObsoleteDateFormats => DateFormat::Asctime,
        _ => DateFormat::Imf,
    };
    // Sent, so the HTTP stack does not add the time it sends the response.
    response_headers.insert(header::DATE, value(http_date(snapshot.date, date_format)));
    response_headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

async fn fault_handler(
    axum::extract::Path(fault): axum::extract::Path<String>,
    axum::extract::State(state): axum::extract::State<FaultState>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let at = Instant::now();
    let fault = RangeFault::from_name(&fault);
    let response = fault_response(fault, &state, &method, &headers);
    let len = fault.map_or(0, |fault| state.content(fault).len() as u64);
    state.log.record(RecordedRequest {
        at,
        method,
        fault,
        range: requested_range(&headers, len),
        if_range: headers
            .get(header::IF_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        status: response.status(),
    });
    response
}

impl FaultState {
    /// What `fault` serves before any fault applies.
    fn content(&self, fault: RangeFault) -> &Bytes {
        if fault == RangeFault::EncodedRepresentation {
            self.encoded
                .get_or_init(|| gzip(&self.content).expect("gzip into memory never fails"))
        } else {
            &self.content
        }
    }
}

fn fault_response(
    fault: Option<RangeFault>,
    state: &FaultState,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let Some(fault) = fault else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if RangeFault::VALIDATED.contains(&fault) {
        return validated_response(fault, state, method, headers);
    }
    let encoded = fault == RangeFault::EncodedRepresentation;
    let content = state.content(fault);
    let len = content.len() as u64;
    let replaced =
        fault == RangeFault::ReplacedAfterShortRange && state.replaced.load(Ordering::SeqCst);
    // The GET route also serves HEAD, which never gets a fault.
    let mut response = if replaced {
        RangeFault::replacement(content).into_response()
    } else if method == Method::HEAD || !headers.contains_key(header::RANGE) {
        content.clone().into_response()
    } else {
        let range = requested_range(headers, len);
        let is_probe = range == Some((0, 0));
        match (fault, range) {
            (RangeFault::IgnoreRange, _) => content.clone().into_response(),
            (RangeFault::Congested, _) => congested(),
            (RangeFault::CongestedOnce, Some((first, last))) if !is_probe => {
                match state.congested_once.fetch_add(1, Ordering::SeqCst) {
                    0 => throttled(content, first, last, len, state.hold.clone()),
                    1 => congested(),
                    _ => throttled(content, first, last, len, None),
                }
            }
            (RangeFault::Unsatisfiable, _) | (_, None) => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{len}"))],
            )
                .into_response(),
            (RangeFault::ShiftedStart, Some((first, last))) if !is_probe => {
                let shifted = if first + 1 < len {
                    first + 1
                } else {
                    first - 1
                };
                partial(content, shifted, last.max(shifted), len)
            }
            (RangeFault::WrongTotal, Some((first, last))) if !is_probe => {
                partial(content, first, last, len + 1)
            }
            (RangeFault::Gzip, Some((first, last))) => {
                let mut response = partial(content, first, last, len);
                response
                    .headers_mut()
                    .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
                response
            }
            (RangeFault::Truncated, Some((first, last)))
                if !is_probe && !state.truncated.swap(true, Ordering::SeqCst) =>
            {
                truncated(content, first, last, len)
            }
            (RangeFault::ReplacedAfterShortRange, Some((first, last)))
                if !is_probe && !state.replaced.swap(true, Ordering::SeqCst) =>
            {
                partial(content, first, first + (last - first) / 2, len)
            }
            (RangeFault::StalledOnce, Some((first, last)))
                if first > 0 && !state.stalled.swap(true, Ordering::SeqCst) =>
            {
                stalled(content, first, last, len, state.hold.clone())
            }
            (_, Some((first, last))) => partial(content, first, last, len),
        }
    };
    if encoded {
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

/// What happened to a connection of a limited server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionEventKind {
    /// Accepted and served.
    Opened,
    /// A served connection closed.
    Closed,
    /// Turned away, as the limit was reached.
    TurnedAway,
}

/// An event of [`ConnectionLog`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionEvent {
    pub at: Instant,
    pub kind: ConnectionEventKind,
    /// The connections served once the event happened.
    pub open: usize,
}

#[derive(Debug, Default)]
struct ConnectionLogInner {
    events: Vec<ConnectionEvent>,
    open: usize,
}

/// The connections a limited server served, closed and turned away, in
/// order.
#[derive(Clone, Debug, Default)]
pub struct ConnectionLog(Arc<Mutex<ConnectionLogInner>>);

impl ConnectionLog {
    pub fn events(&self) -> Vec<ConnectionEvent> {
        self.0.lock().unwrap().events.clone()
    }

    /// The connections served now.
    pub fn open(&self) -> usize {
        self.0.lock().unwrap().open
    }

    /// The most connections served at once.
    pub fn max_open(&self) -> usize {
        self.events()
            .iter()
            .map(|event| event.open)
            .max()
            .unwrap_or(0)
    }

    /// When each connection that was turned away arrived.
    pub fn turned_away(&self) -> Vec<Instant> {
        self.events()
            .iter()
            .filter(|event| event.kind == ConnectionEventKind::TurnedAway)
            .map(|event| event.at)
            .collect()
    }

    /// Serve one more connection, unless `limit` are served.
    fn admit(&self, limit: usize) -> bool {
        let mut inner = self.0.lock().unwrap();
        let admitted = inner.open < limit;
        let kind = if admitted {
            inner.open += 1;
            ConnectionEventKind::Opened
        } else {
            ConnectionEventKind::TurnedAway
        };
        let open = inner.open;
        inner.events.push(ConnectionEvent {
            at: Instant::now(),
            kind,
            open,
        });
        admitted
    }

    fn close(&self) {
        let mut inner = self.0.lock().unwrap();
        inner.open -= 1;
        let open = inner.open;
        inner.events.push(ConnectionEvent {
            at: Instant::now(),
            kind: ConnectionEventKind::Closed,
            open,
        });
    }
}

/// A served connection of a limited server, counted until it closes.
struct LimitedStream {
    inner: tokio::net::TcpStream,
    log: ConnectionLog,
}

impl Drop for LimitedStream {
    fn drop(&mut self) {
        self.log.close();
    }
}

impl tokio::io::AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Accepts every connection, and serves at most `limit` at once.
struct LimitedListener {
    inner: TcpListener,
    limit: usize,
    mode: LimitMode,
    log: ConnectionLog,
    /// The connections [`LimitMode::Hang`] turned away, never answered.
    held: Vec<tokio::net::TcpStream>,
}

impl axum::serve::Listener for LimitedListener {
    type Io = LimitedStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (inner, addr) = axum::serve::Listener::accept(&mut self.inner).await;
            if self.log.admit(self.limit) {
                let log = self.log.clone();
                return (LimitedStream { inner, log }, addr);
            }
            match self.mode {
                LimitMode::Close => drop(inner),
                LimitMode::Hang => self.held.push(inner),
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

#[derive(Clone)]
struct LimitedState {
    content: Bytes,
    bytes_per_sec: u64,
}

async fn limited_handler(
    axum::extract::State(state): axum::extract::State<LimitedState>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    let len = state.content.len() as u64;
    let mut response = if method == Method::HEAD || !headers.contains_key(header::RANGE) {
        state.content.clone().into_response()
    } else if let Some((first, last)) = requested_range(&headers, len) {
        paced(&state.content, first, last, len, None, state.bytes_per_sec)
    } else {
        (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{len}"))],
        )
            .into_response()
    };
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

/// Serve `content` at every path behind a silent connection limit: at most
/// `limit` connections are served at once, and the others are turned away
/// as `mode` says, without any status. Range responses are sent at
/// `bytes_per_sec`, so that connections overlap.
///
/// Also returns the log of the connections, which shows how many were
/// served at once.
pub async fn create_limited_range_server(
    content: Bytes,
    limit: usize,
    mode: LimitMode,
    bytes_per_sec: u64,
) -> anyhow::Result<(u16, tokio::task::JoinHandle<()>, ConnectionLog)> {
    let port =
        portpicker::pick_unused_port().ok_or(anyhow::anyhow!("Failed to pick an unused port"))?;
    let log = ConnectionLog::default();
    let listener = LimitedListener {
        inner: TcpListener::bind(("127.0.0.1", port)).await?,
        limit,
        mode,
        log: log.clone(),
        held: Vec::new(),
    };
    let app = axum::Router::new()
        .fallback(limited_handler)
        .with_state(LimitedState {
            content,
            bytes_per_sec,
        });
    let handle = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap()
    });
    Ok((port, handle, log))
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use reqwest::{
        Client, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, RANGE, RETRY_AFTER,
        },
    };

    use super::*;

    struct ServerGuard(tokio::task::JoinHandle<()>);

    impl Drop for ServerGuard {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    /// Never decodes, whatever features the build enables.
    fn local_client() -> Result<Client> {
        Ok(Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .build()?)
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn server_starts_and_serves_full_content() -> Result<()> {
        let (port, handle) = create_http_server().await?;
        let _guard = ServerGuard(handle);

        let client = local_client()?;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/no_range"))
            .send()
            .await?;

        assert_eq!(resp.status(), StatusCode::OK);

        let headers = resp.headers();
        assert_eq!(
            headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()),
            Some("1048576")
        );

        let body = resp.bytes().await?;
        assert_eq!(body.len(), 1024 * 1024);

        Ok(())
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn server_supports_range_header() -> Result<()> {
        let (port, handle) = create_http_server().await?;
        let _guard = ServerGuard(handle);

        let client = local_client()?;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/range"))
            .header(RANGE, "bytes=0-99")
            .send()
            .await?;

        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);

        let headers = resp.headers();
        let content_range = headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_range.starts_with("bytes 0-99/"),
            "unexpected Content-Range header: {content_range}"
        );

        let body = resp.bytes().await?;
        assert_eq!(body.len(), 100);

        Ok(())
    }

    #[tokio::test]
    #[n0_tracing_test::traced_test]
    async fn no_range_endpoint_ignores_range_header() -> Result<()> {
        let (port, handle) = create_http_server().await?;
        let _guard = ServerGuard(handle);

        let client = local_client()?;
        let resp = client
            .get(format!("http://127.0.0.1:{port}/no_range"))
            .header(RANGE, "bytes=0-99")
            .send()
            .await?;

        // Should return 200 OK, not 206 Partial Content
        assert_eq!(resp.status(), StatusCode::OK);

        let headers = resp.headers();
        // Should not have Content-Range header
        assert!(
            headers.get(CONTENT_RANGE).is_none(),
            "no_range endpoint should not return Content-Range header"
        );

        // Should return full content length
        assert_eq!(
            headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()),
            Some("1048576")
        );

        // Should return full content, not just the requested range
        let body = resp.bytes().await?;
        assert_eq!(body.len(), 1024 * 1024);

        Ok(())
    }

    #[tokio::test]
    async fn range_fault_server_misbehaves_as_told() -> Result<()> {
        let content = Bytes::from_static(b"0123456789");
        let (port, handle) = create_range_fault_server(content.clone()).await?;
        let _guard = ServerGuard(handle);
        let client = local_client()?;
        let get = |fault: RangeFault, range: Option<&'static str>| {
            let mut request = client.get(format!("http://127.0.0.1:{port}{}", fault.path()));
            if let Some(range) = range {
                request = request.header(RANGE, range);
            }
            request.send()
        };
        let header = |resp: &reqwest::Response, name| {
            resp.headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };

        let encoded = gzip(&content)?;
        for fault in RangeFault::ALL {
            let resp = get(fault, None).await?;
            assert_eq!(resp.status(), StatusCode::OK, "{fault:?}");
            assert_eq!(header(&resp, ACCEPT_RANGES).as_deref(), Some("bytes"));
            let body = resp.bytes().await?;
            if fault == RangeFault::EncodedRepresentation {
                assert_eq!(body, encoded);
            } else {
                assert_eq!(body, content, "{fault:?}");
            }
            // HEAD with `Range` gets no fault either.
            let resp = client
                .head(format!("http://127.0.0.1:{port}{}", fault.path()))
                .header(RANGE, "bytes=2-4")
                .send()
                .await?;
            assert_eq!(resp.status(), StatusCode::OK, "HEAD {fault:?}");
        }

        let resp = get(RangeFault::IgnoreRange, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.bytes().await?, content);

        let resp = get(RangeFault::ShiftedStart, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 3-4/10")
        );
        assert_eq!(resp.bytes().await?, "34");
        // One-byte ranges shift too, but not the probe.
        for (range, got) in [
            ("bytes=2-2", "bytes 3-3/10"),
            ("bytes=9-9", "bytes 8-9/10"),
            ("bytes=0-0", "bytes 0-0/10"),
        ] {
            let resp = get(RangeFault::ShiftedStart, Some(range)).await?;
            assert_eq!(
                header(&resp, CONTENT_RANGE).as_deref(),
                Some(got),
                "{range}"
            );
        }

        let resp = get(RangeFault::WrongTotal, Some("bytes=0-0")).await?;
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 0-0/10")
        );
        let resp = get(RangeFault::WrongTotal, Some("bytes=2-4")).await?;
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 2-4/11")
        );

        let resp = get(RangeFault::Unsatisfiable, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(header(&resp, CONTENT_RANGE).as_deref(), Some("bytes */10"));

        let resp = get(RangeFault::Congested, Some("bytes=2-4")).await?;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            header(&resp, RETRY_AFTER),
            Some(CONGESTED_RETRY_AFTER_SECS.to_string())
        );

        let resp = get(RangeFault::Gzip, Some("bytes=2-4")).await?;
        assert_eq!(header(&resp, CONTENT_ENCODING).as_deref(), Some("gzip"));
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 2-4/10")
        );

        // The probe passes; the next range is short, and then the resource
        // is replaced.
        let resp = get(RangeFault::ReplacedAfterShortRange, Some("bytes=0-0")).await?;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let resp = get(RangeFault::ReplacedAfterShortRange, Some("bytes=2-7")).await?;
        assert_eq!(
            header(&resp, CONTENT_RANGE).as_deref(),
            Some("bytes 2-4/10")
        );
        assert_eq!(resp.bytes().await?, "234");
        let replacement = RangeFault::replacement(&content);
        assert_eq!(replacement, "9");
        for range in [Some("bytes=2-7"), None] {
            let resp = get(RangeFault::ReplacedAfterShortRange, range).await?;
            assert_eq!(resp.status(), StatusCode::OK, "{range:?}");
            assert_eq!(resp.bytes().await?, replacement, "{range:?}");
        }

        let resp = get(RangeFault::EncodedRepresentation, Some("bytes=2-4")).await?;
        assert_eq!(header(&resp, CONTENT_ENCODING).as_deref(), Some("gzip"));
        assert_eq!(
            header(&resp, CONTENT_RANGE),
            Some(format!("bytes 2-4/{}", encoded.len()))
        );
        assert_eq!(resp.bytes().await?, encoded.slice(2..5));

        Ok(())
    }

    #[tokio::test]
    async fn range_fault_server_rejects_the_second_range_once_and_logs() -> Result<()> {
        let content = Bytes::from(vec![3u8; DEFAULT_FILE_SIZE]);
        let (port, handle, log) = create_range_fault_server_with_log(content.clone(), None).await?;
        let _guard = ServerGuard(handle);
        let client = local_client()?;
        let url = format!(
            "http://127.0.0.1:{port}{}",
            RangeFault::CongestedOnce.path()
        );
        let get = |range: &'static str| client.get(&url).header(RANGE, range).send();

        // The probe is never rejected nor throttled.
        assert_eq!(
            get("bytes=0-0").await?.status(),
            StatusCode::PARTIAL_CONTENT
        );
        let started = Instant::now();
        let resp = get("bytes=0-").await?;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.bytes().await?, content);
        // The last frame leaves one frame short of the whole body.
        let paced = (DEFAULT_FILE_SIZE - 64 * 1024) as f64 / THROTTLED_BYTES_PER_SEC as f64;
        assert!(started.elapsed() >= Duration::from_secs_f64(paced));
        let resp = get("bytes=5-9").await?;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(RETRY_AFTER).unwrap(),
            &CONGESTED_RETRY_AFTER_SECS.to_string()
        );
        assert_eq!(get("bytes=5-9").await?.bytes().await?, content.slice(5..10));

        let requests = log.requests();
        let seen: Vec<_> = requests
            .iter()
            .map(|request| (request.range, request.status))
            .collect();
        assert_eq!(
            seen,
            [
                (Some((0, 0)), StatusCode::PARTIAL_CONTENT),
                (
                    Some((0, DEFAULT_FILE_SIZE as u64 - 1)),
                    StatusCode::PARTIAL_CONTENT
                ),
                (Some((5, 9)), StatusCode::SERVICE_UNAVAILABLE),
                (Some((5, 9)), StatusCode::PARTIAL_CONTENT),
            ]
        );
        assert!(requests.iter().all(|request| request.method == Method::GET
            && request.fault == Some(RangeFault::CongestedOnce)));
        assert!(requests.windows(2).all(|pair| pair[0].at <= pair[1].at));
        Ok(())
    }

    /// The first range response sends its headers and holds its body at the
    /// gate; the probe and the later requests are answered meanwhile.
    #[tokio::test]
    async fn range_fault_server_holds_the_first_body_at_the_gate() -> Result<()> {
        let content = Bytes::from(vec![5u8; 256 * 1024]);
        let gate = Gate::new();
        let (port, handle, log) =
            create_range_fault_server_with_log(content.clone(), Some(gate.clone())).await?;
        let _guard = ServerGuard(handle);
        let client = local_client()?;
        let url = format!(
            "http://127.0.0.1:{port}{}",
            RangeFault::CongestedOnce.path()
        );
        let get = |range: &'static str| client.get(&url).header(RANGE, range).send();

        // Bounded as a whole: a gate that held the probe or the headers
        // would otherwise hang the test.
        let exchange = async {
            assert_eq!(get("bytes=0-0").await?.bytes().await?, content.slice(..1));
            let held = get("bytes=0-").await?;
            assert_eq!(held.status(), StatusCode::PARTIAL_CONTENT);
            gate.entered().await;
            let resp = get("bytes=5-9").await?;
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(get("bytes=5-9").await?.bytes().await?, content.slice(5..10));

            gate.release();
            assert_eq!(held.bytes().await?, content);
            anyhow::Ok(())
        };
        tokio::time::timeout(Duration::from_secs(10), exchange).await??;
        assert_eq!(log.requests().len(), 4);
        Ok(())
    }

    #[tokio::test]
    async fn range_fault_server_cuts_the_first_range_response() -> Result<()> {
        // Large enough that the headers and the first half leave the server
        // before the body fails.
        let content = Bytes::from(vec![7u8; DEFAULT_FILE_SIZE]);
        let (port, handle) = create_range_fault_server(content.clone()).await?;
        let _guard = ServerGuard(handle);
        let url = format!("http://127.0.0.1:{port}{}", RangeFault::Truncated.path());
        let last = DEFAULT_FILE_SIZE - 1;
        // A fresh client each time, so no request reuses the cut connection.
        let get = |range: String| async {
            local_client()?
                .get(&url)
                .header(RANGE, range)
                .send()
                .await
                .map_err(anyhow::Error::from)
        };

        // The probe is never cut.
        assert_eq!(get("bytes=0-0".into()).await?.bytes().await?.len(), 1);
        let resp = get(format!("bytes=0-{last}")).await?;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert!(resp.bytes().await.is_err(), "the body must be cut off");
        let resp = get(format!("bytes=0-{last}")).await?;
        assert_eq!(resp.bytes().await?, content);

        Ok(())
    }

    /// Two slow responses hold both connections; a third connection is
    /// turned away as the mode says, and served once one of them closed.
    #[tokio::test]
    async fn limited_server_turns_away_connections_past_its_limit() -> Result<()> {
        use super::super::fault::LimitMode;

        let content = Bytes::from(vec![9u8; 256 * 1024]);
        for mode in [LimitMode::Close, LimitMode::Hang] {
            // 64 KiB a second: a range takes seconds, and holds its connection.
            let (port, handle, log) =
                create_limited_range_server(content.clone(), 2, mode, 64 * 1024).await?;
            let _guard = ServerGuard(handle);
            let url = format!("http://127.0.0.1:{port}/file");
            let url = url.as_str();
            let get = |range: &'static str| async move {
                // A client each, so that no connection is shared.
                Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_millis(500))
                    .build()?
                    .get(url)
                    .header(RANGE, range)
                    .send()
                    .await
                    .map_err(anyhow::Error::from)
            };
            let exchange = async {
                let first = get("bytes=0-").await?;
                let second = get("bytes=5-").await?;
                assert_eq!(first.status(), StatusCode::PARTIAL_CONTENT, "{mode:?}");
                assert_eq!(second.status(), StatusCode::PARTIAL_CONTENT, "{mode:?}");
                let turned_away = get("bytes=5-9").await;
                assert!(turned_away.is_err(), "{mode:?}: {turned_away:?}");
                assert_eq!(log.open(), 2);
                assert_eq!(log.turned_away().len(), 1);

                drop(first);
                while log.open() > 1 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                let served = get("bytes=5-9").await?;
                assert_eq!(served.bytes().await?, content.slice(5..10), "{mode:?}");
                drop(second);
                anyhow::Ok(())
            };
            tokio::time::timeout(Duration::from_secs(10), exchange).await??;
            assert_eq!(log.max_open(), 2, "{mode:?}");
            assert_eq!(log.turned_away().len(), 1, "{mode:?}");
            let events = log.events();
            assert!(events.windows(2).all(|pair| pair[0].at <= pair[1].at));
            assert!(
                events
                    .iter()
                    .any(|event| event.kind == ConnectionEventKind::Closed),
                "{mode:?}"
            );
        }
        Ok(())
    }

    /// The paced range is the requested one, and HEAD and a plain `GET`
    /// get the whole resource.
    #[tokio::test]
    async fn limited_server_serves_ranges() -> Result<()> {
        use super::super::fault::LimitMode;

        let content = Bytes::from_static(b"0123456789");
        let (port, handle, log) =
            create_limited_range_server(content.clone(), 1, LimitMode::Close, 1 << 20).await?;
        let _guard = ServerGuard(handle);
        let client = local_client()?;
        let url = format!("http://127.0.0.1:{port}/file");
        let resp = client.get(&url).header(RANGE, "bytes=2-4").send().await?;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers().get(CONTENT_RANGE).unwrap(), "bytes 2-4/10");
        assert_eq!(resp.bytes().await?, "234");
        // The same pooled connection serves the next requests.
        let resp = client.get(&url).send().await?;
        assert_eq!(resp.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
        assert_eq!(resp.bytes().await?, content);
        assert_eq!(client.head(&url).send().await?.status(), StatusCode::OK);
        assert!(log.turned_away().is_empty());
        assert_eq!(log.max_open(), 1);
        Ok(())
    }

    /// The validated faults write `Date` in the three HTTP-date formats,
    /// and their oracle parses dates, so equivalent ones are equal.
    #[test]
    fn validated_dates_in_every_format() {
        let date = SystemTime::UNIX_EPOCH + VALIDATED_DATE;
        let imf = "Fri, 15 Jan 2027 08:00:00 GMT";
        let rfc850 = "Friday, 15-Jan-27 08:00:00 GMT";
        let asctime = "Fri Jan 15 08:00:00 2027";
        assert_eq!(http_date(date, DateFormat::Imf), imf);
        assert_eq!(http_date(date, DateFormat::Rfc850), rfc850);
        assert_eq!(http_date(date, DateFormat::Asctime), asctime);
        let early = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001);
        assert_eq!(
            http_date(early, DateFormat::Asctime),
            "Tue Nov 14 22:13:21 2023"
        );

        let snapshot = Snapshot::of(RangeFault::LastModifiedOneSecondOld, 1, false);
        assert!(snapshot.etag.is_none());
        let last_modified = snapshot.last_modified;
        assert_eq!(
            date.duration_since(last_modified).unwrap(),
            Duration::from_secs(1)
        );
        for format in [DateFormat::Imf, DateFormat::Rfc850, DateFormat::Asctime] {
            assert!(snapshot.holds(&http_date(last_modified, format)));
            assert!(!snapshot.holds(&http_date(date, format)));
        }
        assert!(!snapshot.holds("\"v1\""));
        assert!(!snapshot.holds("yesterday"));

        let snapshot = Snapshot::of(RangeFault::ChangedMidDownload, 1, false);
        assert!(snapshot.holds("\"v1\""));
        assert!(!snapshot.holds("W/\"v1\""));
        assert!(!snapshot.holds("\"v2\""));
        let head = Snapshot::of(RangeFault::HeadValidatorsDiffer, 1, true);
        assert_eq!(head.etag.as_deref(), Some("\"h1\""));
    }
}
