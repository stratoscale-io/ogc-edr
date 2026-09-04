//! Entry point: read the configuration, open the collections, serve.

use std::sync::Arc;

use ogc_edr::api::{self, AppState};
use ogc_edr::catalog::Catalog;
use ogc_edr::config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("ogc_edr=info")),
        )
        .init();

    let config = Config::from_env()?;
    // Opening a collection reads its coordinate axes, so this is where a bad
    // store location or unreachable bucket surfaces — before the port is bound.
    let catalog = Catalog::open(&config.collections).await?;

    let bind = config.bind.clone();
    let state = Arc::new(AppState { catalog, config });
    let app = api::router(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(
        address = %listener.local_addr()?,
        collections = state.catalog.collections.len(),
        "serving OGC API - EDR"
    );
    axum::serve(listener, app).await?;
    Ok(())
}
