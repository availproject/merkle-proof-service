pub mod health;
pub mod justification;
pub mod proof;
pub mod range;

use std::sync::Arc;

use crate::db::Database;
use crate::services::avail::AvailService;
use crate::services::evm::EvmService;

/// Shared application state, passed to all route handlers.
/// All fields are `Arc`/`Clone`-safe for thread-safe sharing across Tokio tasks.
#[derive(Clone)]
pub struct AppState {
    pub evm_service: EvmService,
    pub avail_service: AvailService,
    pub database: Arc<Database>,
}
