use std::collections::HashMap;
use std::sync::Arc;

use jsonrpsee::core::client::ClientT;
use jsonrpsee::core::params::ArrayParams;
use jsonrpsee::http_client::{HttpClient, HttpClientBuilder};

#[derive(Clone)]
pub struct AvailService {
    /// One HTTP client per chain name, created at startup.
    clients: Arc<HashMap<String, HttpClient>>,
}

impl AvailService {
    /// Build the service eagerly: one HTTP transport per configured Avail chain.
    /// Panics at startup if a URL is malformed — fail fast.
    pub fn new(avail_ws_endpoints: &HashMap<String, String>) -> anyhow::Result<Self> {
        let mut clients = HashMap::new();

        for (chain_name, url) in avail_ws_endpoints {
            // The existing config keys say "WS" but plain HTTP RPC endpoints work too.
            // If the URL starts with ws(s):// swap to http(s):// — jsonrpsee HTTP
            // client doesn't speak WebSocket.
            let http_url = url
                .replacen("wss://", "https://", 1)
                .replacen("ws://", "http://", 1);

            let client = HttpClientBuilder::default().build(&http_url)?;
            clients.insert(chain_name.to_lowercase(), client);

            tracing::info!(chain = %chain_name, url = %http_url, "Avail HTTP client ready");
        }

        Ok(Self {
            clients: Arc::new(clients),
        })
    }

    fn get_client(&self, chain_name: &str) -> anyhow::Result<&HttpClient> {
        self.clients
            .get(&chain_name.to_lowercase())
            .ok_or_else(|| anyhow::anyhow!("No Avail client for chain {chain_name}"))
    }

    /// Get the block hash for a given block number.
    pub async fn get_block_hash(
        &self,
        block_number: u32,
        chain_name: &str,
    ) -> anyhow::Result<String> {
        let client = self.get_client(chain_name)?;

        let mut params = ArrayParams::new();
        params.insert(block_number)?;
        let hash: String = client.request("chain_getBlockHash", params).await?;

        Ok(hash)
    }

    /// Get the block number for a given block hash.
    pub async fn get_block_number(
        &self,
        block_hash: &str,
        chain_name: &str,
    ) -> anyhow::Result<u32> {
        let client = self.get_client(chain_name)?;

        let mut params = ArrayParams::new();
        params.insert(block_hash)?;
        let header: serde_json::Value = client.request("chain_getHeader", params).await?;

        let number_hex = header
            .get("number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing number in header"))?;

        let number =
            u32::from_str_radix(number_hex.strip_prefix("0x").unwrap_or(number_hex), 16)?;
        Ok(number)
    }

    /// Fetch the data root from the header extension of a given block.
    pub async fn fetch_data_root(
        &self,
        block_number: u32,
        chain_name: &str,
    ) -> anyhow::Result<[u8; 32]> {
        let client = self.get_client(chain_name)?;

        // Get block hash
        let mut params = ArrayParams::new();
        params.insert(block_number)?;
        let block_hash: String = client.request("chain_getBlockHash", params).await?;

        // Get header with extension
        let mut params = ArrayParams::new();
        params.insert(&block_hash)?;
        let header_json: serde_json::Value =
            client.request("chain_getHeader", params).await?;

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

        let data_root_hex = first_version
            .pointer("/commitment/dataRoot")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Data root not found for block {block_number}"))?;

        let data_root_str = data_root_hex.strip_prefix("0x").unwrap_or(data_root_hex);
        let data_root_bytes = hex::decode(data_root_str)?;

        let mut result = [0u8; 32];
        result.copy_from_slice(&data_root_bytes);
        Ok(result)
    }

    /// Fetch data roots for a range of blocks [start_block, end_block) (end exclusive).
    pub async fn fetch_data_roots_for_range(
        &self,
        start_block: u32,
        end_block: u32,
        chain_name: &str,
    ) -> anyhow::Result<Vec<[u8; 32]>> {
        let mut data_roots = Vec::new();

        for block_number in start_block..end_block {
            let root = self.fetch_data_root(block_number, chain_name).await?;
            data_roots.push(root);
        }

        Ok(data_roots)
    }

    /// Get the list of supported chain names.
    pub fn config_chain_names(&self) -> Vec<String> {
        self.clients.keys().cloned().collect()
    }

    /// Get the finalized head block number.
    pub async fn get_finalized_head_block(&self, chain_name: &str) -> anyhow::Result<u64> {
        let client = self.get_client(chain_name)?;

        let params = ArrayParams::new();
        let hash: String = client.request("chain_getFinalizedHead", params).await?;

        let mut params = ArrayParams::new();
        params.insert(&hash)?;
        let header: serde_json::Value = client.request("chain_getHeader", params).await?;

        let number_hex = header
            .get("number")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing number in header"))?;

        let number =
            u64::from_str_radix(number_hex.strip_prefix("0x").unwrap_or(number_hex), 16)?;
        Ok(number)
    }
}
