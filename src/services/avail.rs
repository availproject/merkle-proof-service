use std::sync::Arc;

use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::ArrayParams;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};

/// Data-root fetches that run at the same time for one range.
const DATA_ROOT_CONCURRENCY: usize = 16;

/// Avail RPC client. A single HTTP transport created at startup for one Avail network.
#[derive(Clone)]
pub struct AvailService {
    client: Arc<HttpClient>,
}

impl AvailService {
    /// Build the service eagerly from a single HTTP(S) RPC URL.
    pub fn new(rpc_url: &str) -> anyhow::Result<Self> {
        let client = HttpClientBuilder::default().build(rpc_url)?;

        tracing::info!("Avail HTTP client ready");

        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// Get the block hash for a given block number.
    pub async fn get_block_hash(&self, block_number: u32) -> anyhow::Result<String> {
        let mut params = ArrayParams::new();
        params.insert(block_number)?;
        let hash: String = self.client.request("chain_getBlockHash", params).await?;
        Ok(hash)
    }

    /// Get the block number for a given block hash.
    pub async fn get_block_number(&self, block_hash: &str) -> anyhow::Result<u32> {
        let mut params = ArrayParams::new();
        params.insert(block_hash)?;
        let header: serde_json::Value = self.client.request("chain_getHeader", params).await?;

        let number_hex = header
            .get("number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing number in header"))?;

        let number = u32::from_str_radix(number_hex.strip_prefix("0x").unwrap_or(number_hex), 16)?;
        Ok(number)
    }

    /// Fetch the data root from the header extension of a given block.
    pub async fn fetch_data_root(&self, block_number: u32) -> anyhow::Result<[u8; 32]> {
        // Get block hash
        let mut params = ArrayParams::new();
        params.insert(block_number)?;
        let block_hash: String = self.client.request("chain_getBlockHash", params).await?;

        // Get header with extension
        let mut params = ArrayParams::new();
        params.insert(&block_hash)?;
        let header_json: serde_json::Value = self.client.request("chain_getHeader", params).await?;

        let extension = header_json
            .get("extension")
            .ok_or_else(|| anyhow::anyhow!("Extension not found for block {block_number}"))?;

        // The extension format varies by version (v2, v3, etc.)
        let ext_obj = extension
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Extension is not an object"))?;

        let first_version = ext_obj
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Extension has no version entries"))?;

        // Extension layout varies by version: older V2/V3 nest the root under
        // `commitment/dataRoot`, while V1 (this chain) exposes `dataRoot` directly.
        let data_root_hex = first_version
            .pointer("/commitment/dataRoot")
            .or_else(|| first_version.pointer("/dataRoot"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Data root not found for block {block_number}"))?;

        let data_root_str = data_root_hex.strip_prefix("0x").unwrap_or(data_root_hex);
        let data_root_bytes = hex::decode(data_root_str)?;

        let mut result = [0u8; 32];
        result.copy_from_slice(&data_root_bytes);
        Ok(result)
    }

    /// Fetch data roots for a range of blocks [start_block, end_block) (end exclusive).
    /// Fetches concurrently (bounded) since each block needs two sequential RPC calls.
    /// Stops at the first failed fetch, because the range then cannot be proven.
    pub async fn fetch_data_roots_for_range(
        &self,
        start_block: u32,
        end_block: u32,
    ) -> anyhow::Result<Vec<[u8; 32]>> {
        let total = end_block.saturating_sub(start_block) as usize;
        let mut data_roots: Vec<[u8; 32]> = vec![[0u8; 32]; total];

        let mut blocks = start_block..end_block;
        let mut set = tokio::task::JoinSet::new();

        loop {
            while set.len() < DATA_ROOT_CONCURRENCY {
                let Some(block_number) = blocks.next() else {
                    break;
                };
                let idx = (block_number - start_block) as usize;
                let svc = self.clone();
                set.spawn(async move {
                    let root = svc.fetch_data_root(block_number).await?;
                    Ok::<(usize, [u8; 32]), anyhow::Error>((idx, root))
                });
            }
            let Some(joined) = set.join_next().await else {
                break;
            };
            // On an error, returning drops the set, which aborts the fetches still running.
            let (idx, root) = joined??;
            data_roots[idx] = root;
        }

        Ok(data_roots)
    }

    /// Get the finalized head block number.
    pub async fn get_finalized_head_block(&self) -> anyhow::Result<u64> {
        let params = ArrayParams::new();
        let hash: String = self
            .client
            .request("chain_getFinalizedHead", params)
            .await?;

        let mut params = ArrayParams::new();
        params.insert(&hash)?;
        let header: serde_json::Value = self.client.request("chain_getHeader", params).await?;

        let number_hex = header
            .get("number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing number in header"))?;

        let number = u64::from_str_radix(number_hex.strip_prefix("0x").unwrap_or(number_hex), 16)?;
        Ok(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_data_root_fetch_stops_at_first_error() {
        // The RPC rejects every call, so the first fetch already fails.
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::TOO_MANY_REQUESTS
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });

        let service = AvailService::new(&format!("http://{addr}/")).unwrap();
        assert!(service.fetch_data_roots_for_range(1, 385).await.is_err());
        // Only the fetches already running may still reach the RPC.
        let calls = calls.load(Ordering::SeqCst);
        assert!(calls <= 2 * DATA_ROOT_CONCURRENCY, "{calls} calls");
    }
}
