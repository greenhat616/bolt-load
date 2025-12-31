use tracing::{level_filters::LevelFilter, *};
use tracing_subscriber::{
    EnvFilter, Layer, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt,
};

pub async fn init_tracing() {
    use tracing_subscriber::Layer;
    let current_crate = env!("CARGO_CRATE_NAME");
    let has_arg_spans = std::env::args().any(|arg| arg == "--spans");

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
    let subscriber = subscriber.with(tokio_console_layer);

    subscriber.init();
}
