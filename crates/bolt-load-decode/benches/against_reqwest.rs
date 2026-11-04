//! Decoding throughput against reqwest's own decoder (T23).
//!
//! Both cases download the same gzip-coded response from a local server and
//! count the decoded bytes: `reqwest` lets the client decode the body,
//! `bolt_load_decode` decodes the body of a client that does not. The target
//! is at least 90 % of reqwest's throughput.
//!
//! Cases (`decode_gzip` group, 32 MiB decoded per iteration):
//! - `counter/*`: little-endian `u64` counters, which compress well;
//! - `text/*`: words drawn from a small list, closer to text.
//!
//! Run with: `cargo bench -p bolt-load-decode --bench against_reqwest`

use std::error::Error;

use bolt_load_decode::{ContentCoding, DecodeOptions, decode};
use bolt_load_tests::adapter::{
    fault::RangeFault, http_server::create_range_fault_server, simple::create_deterministic_content,
};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use futures::{StreamExt, TryStreamExt};
use tokio::runtime::Runtime;

const SIZE: usize = 32 * 1024 * 1024;

type BoxError = Box<dyn Error + Send + Sync>;

/// Words from a list of 62 common ones, picked by a fixed xorshift sequence.
fn text(size: usize) -> Vec<u8> {
    const WORDS: &str = "the of and to in is was that for on with as by at from his an be this \
                         which or had are not but it were have their one all also new first after \
                         its two other more time may has been when who into most over some up \
                         only than out many used city such where later between three them";
    let words: Vec<&str> = WORDS.split_whitespace().collect();
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut text = Vec::with_capacity(size + 16);
    while text.len() < size {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        text.extend_from_slice(words[(state % words.len() as u64) as usize].as_bytes());
        text.push(b' ');
    }
    text.truncate(size);
    text
}

/// The decoded length of the response, decoded by `client`.
async fn decoded_by_reqwest(client: &reqwest::Client, url: &str) -> usize {
    let mut body = client.get(url).send().await.unwrap().bytes_stream();
    let mut len = 0;
    while let Some(chunk) = body.next().await {
        len += chunk.unwrap().len();
    }
    len
}

/// The decoded length of the response, decoded by `decode`.
async fn decoded_by_bolt_load(client: &reqwest::Client, url: &str) -> usize {
    let response = client.get(url).send().await.unwrap();
    assert_eq!(
        response.headers()[reqwest::header::CONTENT_ENCODING],
        "gzip"
    );
    let body = response.bytes_stream().map_err(BoxError::from);
    let mut body = decode(body, &[ContentCoding::Gzip], DecodeOptions::default()).unwrap();
    let mut len = 0;
    while let Some(chunk) = body.next().await {
        len += chunk.unwrap().len();
    }
    len
}

fn bench(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let decoding = reqwest::Client::builder().no_proxy().build().unwrap();
    let plain = reqwest::Client::builder()
        .no_proxy()
        .no_gzip()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("decode_gzip");
    group.throughput(Throughput::Bytes(SIZE as u64));
    group.sample_size(20);
    for (name, content) in [
        ("counter", create_deterministic_content(SIZE)),
        ("text", text(SIZE)),
    ] {
        let (port, server) = runtime
            .block_on(create_range_fault_server(content.into()))
            .unwrap();
        let url = format!(
            "http://127.0.0.1:{port}{}",
            RangeFault::EncodedRepresentation.path()
        );
        // Both decode the whole response.
        assert_eq!(runtime.block_on(decoded_by_reqwest(&decoding, &url)), SIZE);
        assert_eq!(runtime.block_on(decoded_by_bolt_load(&plain, &url)), SIZE);

        group.bench_function(format!("{name}/reqwest"), |b| {
            b.to_async(&runtime)
                .iter(|| decoded_by_reqwest(&decoding, &url));
        });
        group.bench_function(format!("{name}/bolt_load_decode"), |b| {
            b.to_async(&runtime)
                .iter(|| decoded_by_bolt_load(&plain, &url));
        });
        server.abort();
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
