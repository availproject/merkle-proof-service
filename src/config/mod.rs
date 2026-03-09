use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub rpc_urls: HashMap<u64, String>,
    pub avail_rpc_url: String,
    pub avail_network: String,
    pub database_url: String,
    pub server_host: String,
    pub server_port: u16,
    pub deployments: Vec<DeploymentEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DeploymentEntry {
    #[serde(rename = "contractChainId")]
    pub contract_chain_id: u64,
    #[serde(rename = "contractAddress")]
    pub contract_address: String,
    #[serde(rename = "cursorStartBlock")]
    pub cursor_start_block: u64,
    #[serde(rename = "availChainId")]
    pub avail_chain_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeploymentConfig {
    pub deployments: Vec<DeploymentEntry>,
}

impl AppConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().ok();

        let mut rpc_urls = HashMap::new();
        let chain_ids: &[u64] = &[1, 5, 17000, 11155111, 42161, 421614, 8453, 84532, 300, 324];
        for &chain_id in chain_ids {
            let key = format!("RPC_{chain_id}");
            if let Ok(url) = std::env::var(&key) {
                if !url.is_empty() {
                    rpc_urls.insert(chain_id, url);
                }
            }
        }

        let avail_rpc_url = std::env::var("AVAIL_RPC_URL")
            .map_err(|_| anyhow::anyhow!("AVAIL_RPC_URL is required"))?;

        let avail_network = std::env::var("AVAIL_NETWORK")
            .map_err(|_| anyhow::anyhow!("AVAIL_NETWORK is required"))?
            .to_lowercase();

        let database_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgresql://myuser:mypassword@localhost:5432/vectorx-indexer".to_string()
        });

        let server_host = std::env::var("SERVER_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let server_port = std::env::var("SERVER_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3000);

        let deployments_json = include_str!("../../deployments.json");
        let deployment_config: DeploymentConfig = serde_json::from_str(deployments_json)?;

        Ok(Self {
            rpc_urls,
            avail_rpc_url,
            avail_network,
            database_url,
            server_host,
            server_port,
            deployments: deployment_config.deployments,
        })
    }

    pub fn find_deployment(
        &self,
        contract_address: &str,
        chain_id: u64,
    ) -> Option<&DeploymentEntry> {
        self.deployments.iter().find(|d| {
            d.contract_address.to_lowercase() == contract_address.to_lowercase()
                && d.contract_chain_id == chain_id
        })
    }
}
