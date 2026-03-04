use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use alloy::primitives::Address;

use crate::services::evm::get_range_hash;
use crate::services::merkle;

use super::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofQuery {
    pub chain_name: Option<String>,
    pub contract_chain_id: Option<u64>,
    pub contract_address: Option<String>,
    pub block_hash: Option<String>,
    pub block_number: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ProofResponse {
    Success { data: ProofData },
    Error { success: bool, error: serde_json::Value },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofData {
    pub block_number: Option<u32>,
    pub range_hash: String,
    pub data_commitment: String,
    pub merkle_branch: Vec<String>,
    pub index: usize,
    pub total_leaves: u32,
    pub data_root: String,
    pub block_hash: String,
}

fn error_response(msg: &str) -> Json<ProofResponse> {
    Json(ProofResponse::Error {
        success: false,
        error: serde_json::Value::String(msg.to_string()),
    })
}

/// GET /api
///
/// Generates a Merkle proof for a specific Avail block against the VectorX contract's data commitments.
pub async fn get_proof(
    State(state): State<AppState>,
    Query(params): Query<ProofQuery>,
) -> Json<ProofResponse> {
    let chain_name = match &params.chain_name {
        Some(name) => name.clone(),
        None => return error_response("Invalid parameters!"),
    };
    let ethereum_chain_id = match params.contract_chain_id {
        Some(id) => id,
        None => return error_response("Invalid parameters!"),
    };
    let address_str = match &params.contract_address {
        Some(addr) => addr.clone(),
        None => return error_response("Invalid parameters!"),
    };

    tracing::info!(
        chain_name = %chain_name,
        ethereum_chain_id = ethereum_chain_id,
        address = %address_str,
        block_hash = ?params.block_hash,
        block_number = ?params.block_number,
        "Proof request received"
    );

    let requested_block: u32;

    if let Some(hash) = &params.block_hash {
        match state
            .avail_service
            .get_block_number(hash, &chain_name)
            .await
        {
            Ok(num) => requested_block = num,
            Err(_) => return error_response("Invalid block hash!"),
        }
    } else if let Some(num) = params.block_number {
        requested_block = num;
    } else {
        return error_response("No block hash or block number provided!");
    }

    tracing::info!(requested_block = requested_block, "Resolved block number");

    // Parse address
    let address_clean = address_str
        .strip_prefix("0x")
        .unwrap_or(&address_str)
        .to_lowercase();
    let address: Address = match format!("0x{address_clean}").parse() {
        Ok(a) => a,
        Err(_) => return error_response("Invalid contract address!"),
    };

    // Get block range
    let block_range = match state.evm_service.get_block_range(address, ethereum_chain_id).await {
        Ok(range) => range,
        Err(_) => {
            return error_response(
                "Getting the block range covered by the VectorX contract failed!",
            )
        }
    };

    tracing::info!(
        start = block_range.start,
        end = block_range.end,
        "Block range"
    );

    if requested_block < block_range.start || requested_block > block_range.end {
        return error_response(&format!(
            "Requested block {} is not in the range of blocks [{}, {}] contained in the VectorX contract.",
            requested_block, block_range.start, block_range.end
        ));
    }

    // Get block hash and data commitment range concurrently
    let block_hash_fut = state
        .avail_service
        .get_block_hash(requested_block, &chain_name);
    let data_commitment_fut = state
        .evm_service
        .get_data_commitment_range_for_block(ethereum_chain_id, address, requested_block);

    let (block_hash_result, data_commitment_result) =
        tokio::join!(block_hash_fut, data_commitment_fut);

    let requested_block_hash = match block_hash_result {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Failed to get block hash");
            return error_response("Getting block hash failed!");
        }
    };

    let data_commitment_range = match data_commitment_result {
        Ok(Some(range)) => range,
        Ok(None) => {
            return error_response(
                "Requested block is not in the range of blocks contained in the VectorX contract.",
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to get data commitment range");
            return Json(ProofResponse::Error {
                success: false,
                error: serde_json::to_value(format!("{e}")).unwrap_or_default(),
            });
        }
    };

    tracing::info!(
        requested_block_hash = %requested_block_hash,
        "Data commitment range found"
    );

    // Fetch data roots for the range
    let mut data_roots = match state
        .avail_service
        .fetch_data_roots_for_range(
            data_commitment_range.start_block_number + 1,
            data_commitment_range.end_block_number + 1,
            &chain_name,
        )
        .await
    {
        Ok(roots) => roots,
        Err(e) => {
            tracing::error!(error = %e, "Failed to fetch data roots");
            return Json(ProofResponse::Error {
                success: false,
                error: serde_json::to_value(format!("{e}")).unwrap_or_default(),
            });
        }
    };

    tracing::info!(count = data_roots.len(), "Got data roots");

    // Pad to commitment_tree_size with zero-filled entries
    let tree_size = data_commitment_range.commitment_tree_size as usize;
    while data_roots.len() < tree_size {
        data_roots.push([0u8; 32]);
    }

    let index = (requested_block - data_commitment_range.start_block_number - 1) as usize;

    let branch = match merkle::compute_merkle_branch(tree_size, &data_roots, index) {
        Ok(b) => b,
        Err(e) => {
            return Json(ProofResponse::Error {
                success: false,
                error: serde_json::to_value(e).unwrap_or_default(),
            })
        }
    };

    // Verify the branch
    if !merkle::verify_merkle_branch(
        &data_roots[index],
        &branch,
        index,
        &data_commitment_range.data_commitment,
    ) {
        return Json(ProofResponse::Error {
            success: false,
            error: serde_json::to_value(
                "Data commitment does not match the root constructed from the Merkle tree branch!",
            )
            .unwrap_or_default(),
        });
    }

    let range_hash = get_range_hash(
        data_commitment_range.start_block_number,
        data_commitment_range.end_block_number,
    );

    Json(ProofResponse::Success {
        data: ProofData {
            block_number: params.block_number,
            range_hash: format!("0x{}", hex::encode(range_hash)),
            data_commitment: format!("0x{}", hex::encode(data_commitment_range.data_commitment)),
            merkle_branch: branch
                .iter()
                .map(|node| format!("0x{}", hex::encode(node)))
                .collect(),
            index,
            total_leaves: data_commitment_range.commitment_tree_size,
            data_root: format!("0x{}", hex::encode(data_roots[index])),
            block_hash: requested_block_hash,
        },
    })
}
