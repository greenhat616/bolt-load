//! What a range request expects of the resource (design D14).

/// A validator that can make a range request conditional, sent in
/// `If-Range` (RFC 9110 §13.1.5).
///
/// It holds the value as the server sent it, and is sent back unchanged.
/// An HTTP adapter picks it with `bolt_load_http::if_range_validator`.
///
/// `If-Range` takes no other kind (RFC 9110 §13.1.5), so the enum is
/// exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Validator {
    /// A strong entity tag, quotes included, such as `"v1"`.
    StrongEtag(String),
    /// A `Last-Modified` date at least one second older than the `Date` of
    /// the response that carried it.
    LastModified(String),
}

impl Validator {
    /// The value to send in `If-Range`.
    pub fn as_str(&self) -> &str {
        match self {
            Validator::StrongEtag(value) | Validator::LastModified(value) => value,
        }
    }
}

/// What a range request expects of the resource: the representation whose
/// metadata the task retrieved.
///
/// The task builds it once from its own metadata and passes it with every
/// range request, see
/// [`BoltLoadAdapter::range_stream_if`](super::BoltLoadAdapter::range_stream_if).
/// What an adapter caches, or forgets on
/// [`refresh`](super::BoltLoadAdapter::refresh), never changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RangeExpectation {
    /// The total size of the representation.
    pub total: u64,
    /// The validator of the representation, if it has a usable one.
    pub validator: Option<Validator>,
}

impl RangeExpectation {
    /// A representation of `total` bytes, without a validator.
    pub fn new(total: u64) -> Self {
        Self {
            total,
            validator: None,
        }
    }

    /// Set the validator.
    pub fn with_validator(mut self, validator: Option<Validator>) -> Self {
        self.validator = validator;
        self
    }
}
