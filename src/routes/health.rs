use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use alloy::primitives::Address;

use super::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthQuery {
    pub chain_name: Option<String>,
    pub contract_chain_id: Option<u64>,
    pub contract_address: Option<String>,
    pub max_delay_hours: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum HealthResponse {
    Success {
        data: crate::services::evm::HealthInfo,
    },
    Error {
        success: bool,
        error: String,
    },
}

/// GET /api/health
///
/// Returns the health status of a VectorX light client: how far behind the Avail chain head,
/// whether logs have been emitted recently, etc.
pub async fn get_health(
    State(state): State<AppState>,
    Query(params): Query<HealthQuery>,
) -> Json<HealthResponse> {
    let chain_name = match &params.chain_name {
        Some(name) => name.clone(),
        None => {
            return Json(HealthResponse::Error {
                success: false,
                error: "Missing required parameters".to_string(),
            });
        }
    };
    let ethereum_chain_id = match params.contract_chain_id {
        Some(id) => id,
        None => {
            return Json(HealthResponse::Error {
                success: false,
                error: "Missing required parameters".to_string(),
            });
        }
    };
    let address_str = match &params.contract_address {
        Some(addr) => addr.clone(),
        None => {
            return Json(HealthResponse::Error {
                success: false,
                error: "Missing required parameters".to_string(),
            });
        }
    };
    let max_delay_hours = params.max_delay_hours.unwrap_or(4);

    tracing::info!(
        chain_name = %chain_name,
        ethereum_chain_id = ethereum_chain_id,
        address = %address_str,
        "Health request received"
    );

    if chain_name.to_lowercase() != state.avail_network {
        return Json(HealthResponse::Error {
            success: false,
            error: format!(
                "This deployment serves '{}', not '{}'",
                state.avail_network, chain_name
            ),
        });
    }

    let address_clean = address_str
        .strip_prefix("0x")
        .unwrap_or(&address_str)
        .to_lowercase();
    let address: Address = match format!("0x{address_clean}").parse() {
        Ok(a) => a,
        Err(_) => {
            return Json(HealthResponse::Error {
                success: false,
                error: "Invalid contract address".to_string(),
            });
        }
    };

    // Get finalized head from Avail
    let avail_head = match state.avail_service.get_finalized_head_block().await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get Avail finalized head");
            return Json(HealthResponse::Error {
                success: false,
                error: format!("Failed to connect to Avail chain: {e}"),
            });
        }
    };

    let max_delay_seconds = max_delay_hours * 3600;

    match state
        .evm_service
        .get_health_status(address, ethereum_chain_id, avail_head, max_delay_seconds)
        .await
    {
        Ok(info) => Json(HealthResponse::Success { data: info }),
        Err(e) => {
            tracing::error!(error = %e, "Failed to get health status");
            Json(HealthResponse::Error {
                success: false,
                error: format!("Failed to get health status: {e}"),
            })
        }
    }
}
