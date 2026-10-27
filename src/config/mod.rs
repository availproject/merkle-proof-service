use std::collections::HashMap;

use serde::Deserialize;

/// Not `Debug`, so connection settings stay out of logs.
#[derive(Clone)]
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

        let config = Self {
            rpc_urls,
            avail_rpc_url,
            avail_network,
            database_url,
            server_host,
            server_port,
            deployments: deployment_config.deployments,
        };
        config.check_avail_network()?;
        Ok(config)
    }

    /// Find the deployment of a contract that serves this instance's Avail network.
    pub fn find_deployment(
        &self,
        contract_address: &str,
        chain_id: u64,
    ) -> Option<&DeploymentEntry> {
        self.deployments.iter().find(|d| {
            d.contract_address.to_lowercase() == contract_address.to_lowercase()
                && d.contract_chain_id == chain_id
                && d.avail_chain_id.to_lowercase() == self.avail_network
        })
    }

    /// Fail when no deployment serves `avail_network`, because every proof request would fail.
    fn check_avail_network(&self) -> anyhow::Result<()> {
        if self
            .deployments
            .iter()
            .any(|d| d.avail_chain_id.to_lowercase() == self.avail_network)
        {
            return Ok(());
        }
        anyhow::bail!(
            "No deployment in deployments.json serves AVAIL_NETWORK '{}'",
            self.avail_network
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deployment(contract_address: &str, avail_chain_id: &str) -> DeploymentEntry {
        DeploymentEntry {
            contract_chain_id: 11155111,
            contract_address: contract_address.to_string(),
            cursor_start_block: 0,
            avail_chain_id: avail_chain_id.to_string(),
        }
    }

    fn app_config(avail_network: &str, deployments: Vec<DeploymentEntry>) -> AppConfig {
        AppConfig {
            rpc_urls: HashMap::new(),
            avail_rpc_url: String::new(),
            avail_network: avail_network.to_string(),
            database_url: String::new(),
            server_host: String::new(),
            server_port: 0,
            deployments,
        }
    }

    #[test]
    fn test_find_deployment_ignores_other_avail_networks() {
        let config = app_config(
            "turing",
            vec![
                deployment("0xe542db219a7e2b29c7aeaeace242c9a2cd528f96", "turing"),
                deployment("0x054fd961708D8E2B9c10a63F6157c74458889F0a", "hex"),
            ],
        );

        assert!(
            config
                .find_deployment("0xE542DB219A7E2B29C7AEAEACE242C9A2CD528F96", 11155111)
                .is_some()
        );
        assert!(
            config
                .find_deployment("0x054fd961708d8e2b9c10a63f6157c74458889f0a", 11155111)
                .is_none()
        );
    }

    #[test]
    fn test_check_avail_network_needs_a_deployment() {
        let hex_only = app_config(
            "turing",
            vec![deployment(
                "0x054fd961708D8E2B9c10a63F6157c74458889F0a",
                "hex",
            )],
        );
        assert!(hex_only.check_avail_network().is_err());

        let with_turing = app_config(
            "turing",
            vec![deployment(
                "0xe542db219a7e2b29c7aeaeace242c9a2cd528f96",
                "Turing",
            )],
        );
        assert!(with_turing.check_avail_network().is_ok());
    }
}
