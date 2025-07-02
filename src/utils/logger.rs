use tracing::level_filters::LevelFilter;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

pub fn init_tracing() {
    let fmt_layer = tracing_subscriber::fmt::layer().with_level(true);
    let filter_layer = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    let subscriber = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer);

    #[cfg(test)]
    {
        let tokio_console_layer = console_subscriber::spawn();
        let _ = subscriber
            .with(tracing_tracy::TracyLayer::default())
            .with(tokio_console_layer)
            .try_init();
    }

    #[cfg(not(test))]
    {
        let _ = subscriber.try_init();
    }
}
