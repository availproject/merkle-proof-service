mod config;
mod db;
mod routes;
mod services;

use std::sync::Arc;

use axum::Router;
use axum::routing::get;
use tower_http::cors::CorsLayer;
use tracing::{Level, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::FilterFn;
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
    subscriber(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();

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

/// Structured JSON logs, filtered by `env_filter`.
fn subscriber(env_filter: EnvFilter) -> impl Subscriber + Send + Sync {
    tracing_subscriber::registry()
        .with(env_filter)
        .with(limit_transport_logging())
        .with(tracing_subscriber::fmt::layer().json())
}

/// Limit alloy HTTP transport logging to `info`, whatever `RUST_LOG` enables.
fn limit_transport_logging() -> FilterFn {
    FilterFn::new(|metadata| {
        !(metadata.target().starts_with("alloy_transport_http") && *metadata.level() > Level::INFO)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same name, target and field as the span that alloy's `Http::call` opens.
    fn alloy_http_span() -> tracing::Span {
        tracing::debug_span!(
            target: "alloy_transport_http::reqwest_transport",
            "ReqwestTransport",
            url = "http://rpc.example/path-token"
        )
    }

    #[test]
    fn test_logs_never_record_alloy_http_span() {
        for directives in [
            "debug",
            "alloy_transport_http=trace",
            "alloy_transport_http::reqwest_transport=debug",
            "[ReqwestTransport]=debug",
            "info,alloy_transport_http::reqwest_transport=trace",
        ] {
            tracing::subscriber::with_default(subscriber(EnvFilter::new(directives)), || {
                assert!(alloy_http_span().is_disabled(), "{directives}");
            });
        }
    }

    #[test]
    fn test_logs_keep_other_debug_output() {
        tracing::subscriber::with_default(subscriber(EnvFilter::new("debug")), || {
            assert!(!tracing::debug_span!("other").is_disabled());
            assert!(tracing::event_enabled!(
                target: "alloy_transport_http::reqwest_transport",
                Level::INFO
            ));
        });
    }
}
