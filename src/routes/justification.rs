use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use super::AppState;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JustificationQuery {
    pub block_number: Option<i32>,
    pub avail_chain_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum JustificationResponse {
    Success {
        success: bool,
        justification: serde_json::Value,
    },
    Error {
        success: bool,
        error: String,
    },
}

/// GET /api/justification
///
/// Returns the justification data for a given Avail block from the Postgres database.
pub async fn get_justification(
    State(state): State<AppState>,
    Query(params): Query<JustificationQuery>,
) -> impl IntoResponse {
    let block_number = match params.block_number {
        Some(n) => n,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(JustificationResponse::Error {
                    success: false,
                    error: "Missing required parameters: blockNumber and availChainId"
                        .to_string(),
                }),
            )
        }
    };
    let avail_chain_id = match &params.avail_chain_id {
        Some(id) => id.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(JustificationResponse::Error {
                    success: false,
                    error: "Missing required parameters: blockNumber and availChainId"
                        .to_string(),
                }),
            )
        }
    };

    if avail_chain_id.to_lowercase() != state.avail_network {
        return (
            StatusCode::BAD_REQUEST,
            Json(JustificationResponse::Error {
                success: false,
                error: format!(
                    "This deployment serves '{}', not '{}'",
                    state.avail_network, avail_chain_id
                ),
            }),
        );
    }

    tracing::info!(
        block_number = block_number,
        avail_chain_id = %avail_chain_id,
        "Justification request received"
    );

    match state
        .database
        .get_justification(&avail_chain_id, block_number)
        .await
    {
        Ok(Some(row)) => (
            StatusCode::OK,
            Json(JustificationResponse::Success {
                success: true,
                justification: row.data,
            }),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(JustificationResponse::Error {
                success: false,
                error: "No justification found".to_string(),
            }),
        ),
        Err(e) => {
            tracing::error!(error = %e, "Database error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(JustificationResponse::Error {
                    success: false,
                    error: "Database error occurred".to_string(),
                }),
            )
        }
    }
}
