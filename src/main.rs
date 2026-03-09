mod config;
mod db;
mod routes;
mod services;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use config::AppConfig;
use db::Database;
use routes::AppState;
use services::avail::AvailService;
use services::evm::EvmService;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing (engineering-toolkit standard: structured JSON logs with env filter)
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    tracing::info!("Starting merkle-proof-service");

    let config = Arc::new(AppConfig::from_env()?);
    let database = Arc::new(Database::new(&config.database_url).await?);

    let evm_service = EvmService::new(config.clone());
    let avail_service = AvailService::new(&config.avail_rpc_url)?;

    tracing::info!(network = %config.avail_network, "Avail network configured");

    let state = AppState {
        evm_service,
        avail_service,
        avail_network: config.avail_network.clone(),
        database,
    };

    let app = Router::new()
        .route("/api", get(routes::proof::get_proof))
        .route("/api/range", get(routes::range::get_range))
        .route(
            "/api/justification",
            get(routes::justification::get_justification),
        )
        .route("/api/health", get(routes::health::get_health))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let bind_addr = format!("{}:{}", config.server_host, config.server_port);
    tracing::info!(address = %bind_addr, "Listening");

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
