use std::sync::Once;

pub type Result = eyre::Result<()>;

static INIT: Once = Once::new();

pub fn init() {
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .pretty()
            .init();

        color_eyre::install().unwrap();
    });
}
