use std::time::Instant;

use bolt_load::{
    adapter::BoltLoadAdapter,
    runtime::ThreadedRuntimeImpl,
    task::{DownloadMode, TaskBuilder},
};
use bolt_load_tests::adapter::simple::{
    SimpleTestAdapter, SimpleTestAdapterBuilder, calculate_blake3,
};
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    metrics::{MeterProviderBuilder, PeriodicReader, SdkMeterProvider},
    trace::{RandomIdGenerator, Sampler, SdkTracerProvider},
};
use opentelemetry_semantic_conventions::{
    SCHEMA_URL,
    attribute::{SERVICE_NAME, SERVICE_VERSION},
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

pub async fn init_tracing(opentelemetry_ptr: &mut *mut OtelGuard) {
    let current_crate = env!("CARGO_CRATE_NAME");
    let has_arg_spans = std::env::args().any(|arg| arg == "--spans");
    let has_arg_opentelemetry = std::env::args().any(|arg| arg == "--opentelemetry");

    // let fmt_layer = tracing_subscriber::fmt::layer().with_level(true);
    // let fmt_layer = if has_arg_spans {
    //     fmt_layer.with_span_events(FmtSpan::ENTER | FmtSpan::CLOSE)
    // } else {
    //     fmt_layer
    // }
    // .with_filter(
    //     EnvFilter::builder()
    //         .with_default_directive(LevelFilter::WARN.into())
    //         .parse(format!("bolt_load=trace,{current_crate}=trace"))
    //         .unwrap(),
    // );

    let filter_layer = EnvFilter::builder()
        .with_default_directive(LevelFilter::TRACE.into())
        .from_env_lossy();

    let subscriber = tracing_subscriber::registry().with(filter_layer);
    // .with(fmt_layer);
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

#[tokio::main]
async fn main() {
    let mut guard_ptr = std::ptr::null_mut();
    init_tracing(&mut guard_ptr).await;
    let file_size = 1024 * 1024 * 1024 * 10; // 10GB for reasonable test time
    let temp_dir = TempDir::new().unwrap();

    // Set up adapter with range support for concurrent downloading
    let adapter = SimpleTestAdapterBuilder::new()
        .support_range(true)
        .content_size(file_size)
        .max_per_stream_speed(1024 * 1024 * 50) // 50MB/s per stream
        .build()
        .unwrap();

    info!("adapter: {:?}", adapter);

    // Test concurrent mode
    let runtime = ThreadedRuntimeImpl::new_tokio_rt();
    let expected_hash = adapter.expected_hash().to_string();
    let save_path = temp_dir.path().join("concurrent_performance_test.bin");

    let mut concurrent_task = TaskBuilder::default()
        .adapter(Box::new(adapter) as Box<dyn BoltLoadAdapter + Send>)
        .save_path(save_path.clone())
        .prefer_mode(DownloadMode::Concurrent)
        .cancel_token(CancellationToken::new())
        .threaded_runtime(runtime.clone())
        .build()
        .await
        .unwrap();

    info!(
        "🏁 Starting concurrent performance test ({}MB)...",
        file_size / (1024 * 1024)
    );

    // Run the download and measure time
    let start_time = Instant::now();
    concurrent_task.run().await.unwrap();
    concurrent_task
        .wait()
        .await
        .expect("concurrent task should succeed");
    let concurrent_duration = start_time.elapsed();

    let concurrent_speed = (file_size as f64 / concurrent_duration.as_secs_f64()).round();

    // Verify downloaded file
    let content = std::fs::read(&save_path).unwrap();
    let actual_hash = calculate_blake3(&content);
    assert_eq!(
        actual_hash, expected_hash,
        "Concurrent file hash should be correct"
    );

    info!("✓ Concurrent performance test completed");
    info!(
        "
    - Concurrent result:
        - Measured Speed: {} MB/s
        - Duration: {:?}
        - Hash: {}
        - File size: {} bytes",
        concurrent_speed / (1024.0 * 1024.0),
        concurrent_duration,
        actual_hash,
        file_size
    );

    // Additional assertions
    assert!(save_path.exists(), "Downloaded file should exist");
    assert_eq!(
        content.len(),
        file_size,
        "Downloaded file should have correct size"
    );

    info!("  - Concurrent mode completed successfully with matching hash");
}
