//! How an HTTP status maps to an [`AdapterError`].
//!
//! The mapping is part of the adapter contract: the task degrades on
//! [`UnretryableError::Congestion`] and retries [`AdapterError::Retryable`],
//! so an HTTP adapter should classify statuses through [`classify_status`].

use std::time::Duration;

use bolt_load_core::adapter::{AdapterError, RetryableError, UnretryableError};

/// The longest `Retry-After` delay honoured; longer values are clamped.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Map an error status to an [`AdapterError`].
///
/// `retry_after` is the server's parsed `Retry-After` delay. It is kept
/// only for 429 and 503, clamped to [`MAX_RETRY_AFTER`].
pub fn classify_status(status: u16, retry_after: Option<Duration>) -> AdapterError {
    match status {
        401 | 403 => UnretryableError::Unauthorized {
            status,
            message: if status == 401 {
                "unauthorized"
            } else {
                "forbidden"
            }
            .to_owned(),
        }
        .into(),
        404 | 410 => UnretryableError::NotFound { status }.into(),
        408 => RetryableError::Status { status }.into(),
        416 => UnretryableError::RangeNotSatisfiable { total: None }.into(),
        429 | 503 => UnretryableError::Congestion {
            status,
            retry_after: retry_after.map(|delay| delay.min(MAX_RETRY_AFTER)),
        }
        .into(),
        500..=599 => RetryableError::Status { status }.into(),
        _ => UnretryableError::Status { status }.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(status: u16) -> AdapterError {
        classify_status(status, None)
    }

    #[test]
    fn statuses_map_to_the_documented_variants() {
        use AdapterError::{Retryable, Unretryable};

        for status in [401, 403] {
            assert!(matches!(
                classify(status),
                Unretryable { source: UnretryableError::Unauthorized { status: s, .. } } if s == status
            ));
        }
        for status in [404, 410] {
            assert!(matches!(
                classify(status),
                Unretryable { source: UnretryableError::NotFound { status: s } } if s == status
            ));
        }
        for status in [408, 500, 599] {
            assert!(matches!(
                classify(status),
                Retryable { source: RetryableError::Status { status: s } } if s == status
            ));
        }
        assert!(matches!(
            classify(416),
            Unretryable {
                source: UnretryableError::RangeNotSatisfiable { total: None }
            }
        ));
        for status in [429, 503] {
            assert!(matches!(
                classify(status),
                Unretryable { source: UnretryableError::Congestion { status: s, retry_after: None } } if s == status
            ));
        }
        for status in [400, 409, 412, 418] {
            assert!(matches!(
                classify(status),
                Unretryable { source: UnretryableError::Status { status: s } } if s == status
            ));
        }
    }

    #[test]
    fn retry_after_is_kept_for_congestion_only_and_clamped() {
        let retry_after = |status, delay| match classify_status(status, Some(delay)) {
            AdapterError::Unretryable {
                source: UnretryableError::Congestion { retry_after, .. },
            } => Some(retry_after),
            _ => None,
        };
        let five = Duration::from_secs(5);
        assert_eq!(retry_after(429, five), Some(Some(five)));
        assert_eq!(retry_after(503, five), Some(Some(five)));
        assert_eq!(
            retry_after(503, Duration::from_secs(999)),
            Some(Some(MAX_RETRY_AFTER))
        );
        assert_eq!(retry_after(500, five), None);
    }
}
