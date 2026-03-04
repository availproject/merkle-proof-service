use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use alloy::primitives::Address;

use super::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeQuery {
    pub contract_chain_id: Option<u64>,
    pub contract_address: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RangeResponse {
    Success {
        data: crate::services::evm::RangeInfo,
    },
    Error {
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// GET /api/range
///
/// Returns the range of Avail blocks covered by the VectorX contract.
pub async fn get_range(
    State(state): State<AppState>,
    Query(params): Query<RangeQuery>,
) -> Json<RangeResponse> {
    let ethereum_chain_id = match params.contract_chain_id {
        Some(id) => id,
        None => {
            return Json(RangeResponse::Error {
                success: false,
                error: None,
            })
        }
    };
    let address_str = match &params.contract_address {
        Some(addr) => addr.clone(),
        None => {
            return Json(RangeResponse::Error {
                success: false,
                error: None,
            })
        }
    };

    tracing::info!(
        ethereum_chain_id = ethereum_chain_id,
        address = %address_str,
        "Range request received"
    );

    let address_clean = address_str
        .strip_prefix("0x")
        .unwrap_or(&address_str)
        .to_lowercase();
    let address: Address = match format!("0x{address_clean}").parse() {
        Ok(a) => a,
        Err(_) => {
            return Json(RangeResponse::Error {
                success: false,
                error: Some("Invalid contract address".to_string()),
            })
        }
    };

    match state
        .evm_service
        .get_block_range(address, ethereum_chain_id)
        .await
    {
        Ok(range) => Json(RangeResponse::Success { data: range }),
        Err(_) => Json(RangeResponse::Error {
            success: false,
            error: Some(
                "Failed to get block range for requested block! This means that the specified contract is not registered in this service."
                    .to_string(),
            ),
        }),
    }
}
