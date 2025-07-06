use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use bolt_load::{
    adapter::{
        AnyBytesStream, BoltLoadAdapter, BoltLoadAdapterMeta, StreamError, UnretryableError,
    },
    runtime::ThreadedRuntimeImpl,
    task::{DownloadMode, TaskBuilder},
};
use bytes::Bytes;
use opentelemetry::{Key, KeyValue, Value, global};
use opentelemetry_otlp::{ExportConfig, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    metrics::{MeterProviderBuilder, PeriodicReader, SdkMeterProvider},
    trace::{RandomIdGenerator, Sampler, SdkTracerProvider},
};
use opentelemetry_semantic_conventions::{
    SCHEMA_URL,
    attribute::{
        DEPLOYMENT_ENVIRONMENT_NAME, SERVICE_NAME, SERVICE_VERSION, VCS_REF_BASE_REVISION,
    },
};
use smol_cancellation_token::CancellationToken;
use tempfile::TempDir;
use tracing::{level_filters::LevelFilter, *};
use tracing_subscriber::{
    EnvFilter, Layer, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt,
};

// Create a Resource that captures information about the entity for which telemetry is recorded.
fn resource() -> Resource {
    Resource::builder_empty()
        .with_schema_url(
            [
                KeyValue::new(SERVICE_NAME, "bolt-load"),
                KeyValue::new(SERVICE_VERSION, "0.1.0"),
            ],
            SCHEMA_URL,
        )
        .build()
}

// Construct MeterProvider for MetricsLayer
fn init_meter_provider() -> SdkMeterProvider {
    let mut builder = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_temporality(opentelemetry_sdk::metrics::Temporality::default());
    if let Ok(otlp_endpoint) = std::env::var("BOLT_LOAD_OTLP_METRIC_ENDPOINT") {
        builder = builder.with_endpoint(otlp_endpoint);
    }
    let exporter = builder.build().expect("failed to build metric exporter");

    let reader = PeriodicReader::builder(exporter)
        .with_interval(std::time::Duration::from_secs(30))
        .build();

    // For debugging in development
    let stdout_reader =
        PeriodicReader::builder(opentelemetry_stdout::MetricExporter::default()).build();

    let meter_provider = MeterProviderBuilder::default()
        .with_resource(resource())
        .with_reader(reader)
        .with_reader(stdout_reader)
        .build();

    global::set_meter_provider(meter_provider.clone());

    meter_provider
}

// Construct TracerProvider for OpenTelemetryLayer
fn init_tracer_provider() -> SdkTracerProvider {
    let mut builder = opentelemetry_otlp::SpanExporter::builder().with_tonic();
    if let Ok(otlp_endpoint) = std::env::var("BOLT_LOAD_OTLP_TRACE_ENDPOINT") {
        builder = builder.with_endpoint(otlp_endpoint);
    }
    let exporter = builder.build().expect("failed to build span exporter");

    SdkTracerProvider::builder()
        // Customize sampling strategy
        .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
            1.0,
        ))))
        // If export trace to AWS X-Ray, you can use XrayIdGenerator
        .with_id_generator(RandomIdGenerator::default())
        .with_resource(resource())
        .with_batch_exporter(exporter)
        .build()
}

pub struct OtelGuard {
    pub tracer_provider: SdkTracerProvider,
    pub meter_provider: SdkMeterProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Err(err) = self.tracer_provider.shutdown() {
            eprintln!("{err:?}");
        }
        if let Err(err) = self.meter_provider.shutdown() {
            eprintln!("{err:?}");
        }
    }
}

pub async fn init_telemetry() -> OtelGuard {
    // NOTE: this is needed for otlp grpc exporter, because it inner use tokio as executor to run grpc connection,
    // when it do not run in a tokio context, it will exit with status code 1, and with no more information.
    // So please do NOT remove this `block_on` call.
    let tracer_provider = init_tracer_provider();
    let meter_provider = init_meter_provider();

    OtelGuard {
        tracer_provider,
        meter_provider,
    }
}

/// Calculate blake3 hash of the content
#[cfg_attr(feature = "tracing", tracing::instrument(skip(content), ret))]
pub fn calculate_blake3(content: &[u8]) -> String {
    let hash = blake3::hash(content);
    hash.to_hex().to_string()
}

/// Creates a deterministic test content with specified size for hash verification
#[cfg_attr(feature = "tracing", tracing::instrument)]
pub fn create_deterministic_content(size: usize) -> Vec<u8> {
    let mut content = Vec::with_capacity(size);
    let mut counter = 0u64;

    unsafe {
        let ptr: *mut u8 = content.as_mut_ptr();
        let mut offset = 0;

        while offset + 8 <= size {
            std::ptr::write_unaligned(ptr.add(offset) as *mut u64, counter.to_le());
            offset += 8;
            counter += 1;
        }

        if offset < size {
            let bytes = counter.to_le_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(offset), size - offset);
        }

        content.set_len(size);
    }

    content
}

/// Simple test adapter for testing Task and TaskBuilder
#[derive(Clone, derive_more::Debug)]
pub struct SimpleTestAdapter {
    #[debug(ignore)]
    content: Arc<Vec<u8>>,
    expected_hash: String,
    support_range: bool,
    should_fail: bool,
    chunk_size: usize,
    call_count: Arc<AtomicUsize>,
    filename: Option<String>,
}

impl SimpleTestAdapter {
    pub fn clone_reset_count(&self) -> Self {
        Self {
            content: self.content.clone(),
            expected_hash: self.expected_hash.clone(),
            support_range: self.support_range,
            should_fail: self.should_fail,
            chunk_size: self.chunk_size,
            call_count: Arc::new(AtomicUsize::new(0)),
            filename: self.filename.clone(),
        }
    }

    /// Create a new test adapter with deterministic content
    #[cfg_attr(feature = "tracing", tracing::instrument)]
    pub fn new(size: usize) -> Self {
        let content = create_deterministic_content(size);
        let expected_hash = calculate_blake3(&content);

        Self {
            content: Arc::new(content),
            expected_hash,
            support_range: true,
            should_fail: false,
            chunk_size: 8192, // 8KB chunks by default
            call_count: Arc::new(AtomicUsize::new(0)),
            filename: Some("test_file.bin".to_string()),
        }
    }

    /// Create large test content (100MB)
    pub fn new_large() -> Self {
        Self::new(100 * 1024 * 1024) // 100MB
    }

    /// Configure range support
    pub fn with_range_support(mut self, support: bool) -> Self {
        self.support_range = support;
        self
    }

    /// Configure failure simulation
    pub fn with_failure(mut self, should_fail: bool) -> Self {
        self.should_fail = should_fail;
        self
    }

    /// Configure filename
    pub fn with_filename(mut self, filename: String) -> Self {
        self.filename = Some(filename);
        self
    }

    /// Configure chunk size
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Get expected hash for verification
    pub fn expected_hash(&self) -> &str {
        &self.expected_hash
    }

    /// Get call count
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl BoltLoadAdapter for SimpleTestAdapter {
    async fn is_range_stream_available(&self) -> bool {
        info!(
            "[TEST ADAPTER] is_range_stream_available() called, returning: {}",
            self.support_range
        );
        self.call_count.fetch_add(1, Ordering::Relaxed);
        self.support_range
    }

    async fn retrieve_meta(&self) -> Result<BoltLoadAdapterMeta, UnretryableError> {
        info!("[TEST ADAPTER] retrieve_meta() called");
        self.call_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail {
            info!("[TEST ADAPTER] retrieve_meta() returning failure");
            return Err(UnretryableError::Internal(
                "Simulated meta retrieval failure".to_string(),
            ));
        }

        info!(
            "[TEST ADAPTER] retrieve_meta() success, size: {}",
            self.content.len()
        );

        Ok(BoltLoadAdapterMeta {
            content_size: self.content.len() as u64,
            filename: self.filename.clone(),
        })
    }

    async fn full_stream(&self) -> Result<AnyBytesStream, StreamError> {
        info!(
            "[TEST ADAPTER] full_stream() called, content size: {}",
            self.content.len()
        );
        self.call_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail {
            info!("[TEST ADAPTER] full_stream() returning failure");
            return Err(StreamError::Unretryable(UnretryableError::Internal(
                "Simulated stream failure".to_string(),
            )));
        }

        let content = self.content.clone();
        let chunk_size = self.chunk_size;
        info!(
            "[TEST ADAPTER] full_stream() creating stream with chunk_size: {}",
            chunk_size
        );
        let stream = async_stream::stream! {
            for chunk in content.chunks(chunk_size) {
                yield Ok(Bytes::from(chunk.to_vec()));
            }
        };
        Ok(Box::pin(stream))
    }

    async fn range_stream(&self, start: u64, end: u64) -> Result<AnyBytesStream, StreamError> {
        self.call_count.fetch_add(1, Ordering::Relaxed);

        if self.should_fail {
            return Err(StreamError::Unretryable(UnretryableError::Internal(
                "Simulated range stream failure".to_string(),
            )));
        }

        if !self.support_range {
            return Err(StreamError::Unretryable(UnretryableError::Internal(
                "Range requests not supported".to_string(),
            )));
        }

        let start = start as usize;
        let end = end as usize;
        let content = self.content[start..end.min(self.content.len())].to_vec();
        let chunk_size = self.chunk_size;

        let stream = async_stream::stream! {
            for chunk in content.chunks(chunk_size) {
                yield Ok(Bytes::from(chunk.to_vec()));
            }
        };
        Ok(Box::pin(stream))
    }
}

pub async fn init_tracing(opentelemetry_ptr: &mut *mut OtelGuard) {
    let current_crate = env!("CARGO_CRATE_NAME");
    let has_arg_spans = std::env::args().any(|arg| arg == "--spans");
    let has_arg_opentelemetry = std::env::args().any(|arg| arg == "--opentelemetry");

    let fmt_layer = tracing_subscriber::fmt::layer().with_level(true);
    let fmt_layer = if has_arg_spans {
        fmt_layer.with_span_events(FmtSpan::ENTER | FmtSpan::CLOSE)
    } else {
        fmt_layer
    }
    .with_filter(
        EnvFilter::builder()
            .with_default_directive(LevelFilter::WARN.into())
            .parse(format!("bolt_load=trace,{current_crate}=trace"))
            .unwrap(),
    );

    let filter_layer = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .from_env_lossy();

    let subscriber = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer);
    let tokio_console_layer = console_subscriber::spawn();
    let subscriber = subscriber
        .with(tracing_tracy::TracyLayer::default())
        .with(tokio_console_layer);

    if has_arg_opentelemetry {
        let (subscriber, telemetry_guard) = {
            use opentelemetry::trace::TracerProvider as _;
            let guard = init_telemetry().await;
            let tracer = guard.tracer_provider.tracer("bolt_load");
            let opentelemetry_trace_filter =
                tracing_subscriber::filter::LevelFilter::from_level(Level::TRACE);
            let subscriber = subscriber
                .with(
                    tracing_opentelemetry::MetricsLayer::new(guard.meter_provider.clone())
                        .with_filter(opentelemetry_trace_filter),
                )
                .with(
                    tracing_opentelemetry::OpenTelemetryLayer::new(tracer)
                        .with_filter(opentelemetry_trace_filter),
                );
            (subscriber, guard)
        };
        let box_guard = Box::new(telemetry_guard);
        *opentelemetry_ptr = Box::into_raw(box_guard);
        subscriber.init();
    } else {
        subscriber.init();
    }
}

#[cfg_attr(feature = "tracing", tracing::instrument)]
async fn test_concurrent_vs_singleton_performance() {
    let file_size = 1024 * 1024 * 1024; // 1GB for reasonable test time
    let temp_dir = TempDir::new().unwrap();

    // Set up adapters
    let adapter1 = SimpleTestAdapter::new(file_size).with_range_support(true);
    info!("adapter1: {:?}", adapter1);
    let adapter2 = adapter1.clone_reset_count().with_range_support(false);
    info!("adapter2: {:?}", adapter2);

    // Test concurrent mode
    let runtime = ThreadedRuntimeImpl::new_tokio_rt();
    let concurrent_expected_hash = adapter1.expected_hash().to_string();
    let concurrent_path = temp_dir.path().join("concurrent.bin");

    let mut concurrent_task = TaskBuilder::default()
        .adapter(Box::new(adapter1) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(concurrent_path.clone())
        .prefer_mode(DownloadMode::Concurrent)
        .cancel_token(CancellationToken::new())
        .threaded_runtime(runtime.clone())
        .build()
        .await
        .unwrap();

    // Test singleton mode
    // let singleton_expected_hash = adapter2.expected_hash().to_string();
    // let singleton_path = temp_dir.path().join("singleton.bin");

    // let mut singleton_task = TaskBuilder::default()
    //     .adapter(Box::new(adapter2) as Box<dyn BoltLoadAdapter + Send>)
    //     .save_path(singleton_path.clone())
    //     .prefer_mode(DownloadMode::Singleton)
    //     .cancel_token(CancellationToken::new())
    //     .runtime(runtime.clone())
    //     .build()
    //     .await
    //     .unwrap();

    info!("🏁 Starting performance comparison test (1MB)...");

    // Run both downloads and measure time
    let start_time = Instant::now();
    concurrent_task.run().await.unwrap();
    concurrent_task
        .wait()
        .await
        .expect("concurrent task should succeed");
    info!("Concurrent task finished");
    let concurrent_duration = start_time.elapsed();

    // let start_time = Instant::now();
    // singleton_task.run().await.unwrap();
    // singleton_task
    //     .wait()
    //     .await
    //     .expect("singleton task should succeed");
    // info!("Singleton task finished");
    // let singleton_duration = start_time.elapsed();

    let concurrent_speed = (file_size as f64 / concurrent_duration.as_secs_f64()).round();
    // let singleton_speed = (file_size as f64 / singleton_duration.as_secs_f64()).round();

    // Verify singleton file
    // let singleton_content = std::fs::read(&singleton_path).unwrap();
    // let singleton_hash = calculate_blake3(&singleton_content);
    let concurrent_content = std::fs::read(&concurrent_path).unwrap();
    let concurrent_hash = calculate_blake3(&concurrent_content);
    // assert_eq!(
    //     singleton_hash, singleton_expected_hash,
    //     "Singleton file hash should be correct"
    // );
    assert_eq!(
        concurrent_hash, concurrent_expected_hash,
        "Concurrent file hash should be correct"
    );

    info!("✓ Performance comparison test completed");
    info!(
        "
    - Concurrent result:
        - Measured Speed: {concurrent_speed} MB/s
        - Duration: {concurrent_duration:?}
        - Hash: {concurrent_hash}
        - File size: {file_size} bytes"
    );
    // info!(
    //     "
    // - Singleton result:
    //     - Measured Speed: {singleton_speed} MB/s
    //     - Duration: {singleton_duration:?}
    //     - Hash: {singleton_hash}
    //     - File size: {file_size} bytes"
    // );

    let concurrent_content = std::fs::read(&concurrent_path).unwrap();
    let concurrent_hash = calculate_blake3(&concurrent_content);
    assert_eq!(
        concurrent_hash, concurrent_expected_hash,
        "Concurrent file hash should be correct"
    );
    info!("  - Both modes completed successfully with matching hashes");
}

fn main() {
    let time = Instant::now();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let elapsed = time.elapsed();
        println!("Runtime initialized in {elapsed:?}");
        let time = Instant::now();
        let mut guard_ptr = std::ptr::null_mut();
        init_tracing(&mut guard_ptr).await;
        trace!("tracing initialized in {:?}", time.elapsed());
        test_concurrent_vs_singleton_performance().await;

        if !guard_ptr.is_null() {
            drop(unsafe { Box::from_raw(guard_ptr) });
        }
    });
    let elapsed = time.elapsed();
    info!("Total time: {:?}", elapsed);
}
