//! Clients the adapter integration tests run every case on.

use bolt_load::adapter::{AnyAdapter, DynBoltLoadAdapter};
use bolt_load_adapter::{reqwest::IntoReqwestAdapter, ureq2::IntoUreqAdapter};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    Reqwest,
    /// reqwest with its gzip decoder, which hides `Content-Encoding`.
    DecodingReqwest,
    Ureq2,
}

impl Client {
    pub const ALL: [Self; 3] = [Self::Reqwest, Self::DecodingReqwest, Self::Ureq2];

    pub fn adapter(self, url: Url) -> Box<AnyAdapter> {
        match self {
            Client::Reqwest | Client::DecodingReqwest => {
                let mut client = reqwest::Client::builder().no_proxy();
                if self == Client::Reqwest {
                    client = client.no_gzip().no_brotli().no_zstd().no_deflate();
                }
                DynBoltLoadAdapter::new_box(
                    client
                        .build()
                        .unwrap()
                        .into_reqwest_adapter((reqwest::Method::GET, url)),
                )
            }
            Client::Ureq2 => {
                DynBoltLoadAdapter::new_box(ureq2::Agent::new().into_ureq_adapter(("get", url)))
            }
        }
    }
}
