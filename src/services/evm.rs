use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{Filter, Log};
use alloy::sol;
use alloy::sol_types::SolEvent;

use crate::config::AppConfig;

sol! {
    #[sol(rpc)]
    contract VectorX {
        function latestBlock() external view returns (uint32);
    }

    event HeaderRangeCommitmentStored(
        uint32 startBlock,
        uint32 endBlock,
        bytes32 dataCommitment,
        bytes32 stateCommitment,
        uint32 headerRangeCommitmentTreeSize
    );

    event HeadUpdate(
        uint32 blockNumber,
        bytes32 headerHash
    );
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DataCommitmentRange {
    pub start_block_number: u32,
    pub end_block_number: u32,
    pub data_commitment: [u8; 32],
    pub state_commitment: [u8; 32],
    pub commitment_tree_size: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeInfo {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HealthInfo {
    #[serde(rename = "blocksBehindHead")]
    pub blocks_behind_head: i64,
    #[serde(rename = "ethBlocksSinceLastLog")]
    pub eth_blocks_since_last_log: u64,
    #[serde(rename = "lastLogTimestamp")]
    pub last_log_timestamp: u64,
    #[serde(rename = "logEmitted")]
    pub log_emitted: bool,
}

#[derive(Clone)]
pub struct EvmService {
    config: Arc<AppConfig>,
    /// One provider per chain ID, created at startup.
    providers: Arc<HashMap<u64, RootProvider>>,
}

impl EvmService {
    /// Build the service eagerly: one HTTP provider per configured RPC chain.
    pub fn new(config: Arc<AppConfig>) -> Self {
        let mut providers = HashMap::new();

        for (&chain_id, rpc_url) in &config.rpc_urls {
            match rpc_url.parse::<url::Url>() {
                Ok(url) => {
                    providers.insert(chain_id, RootProvider::new_http(url));
                    tracing::info!(chain_id, "EVM HTTP provider ready");
                }
                Err(e) => {
                    tracing::warn!(chain_id, error = %e, "Skipping invalid RPC URL");
                }
            }
        }

        Self {
            config,
            providers: Arc::new(providers),
        }
    }

    fn get_provider(&self, chain_id: u64) -> anyhow::Result<&RootProvider> {
        self.providers
            .get(&chain_id)
            .ok_or_else(|| anyhow::anyhow!("No provider for chain {chain_id}"))
    }

    /// Query event logs from the contract.
    pub async fn query_logs(
        &self,
        chain_id: u64,
        contract_address: Address,
        from_block: u64,
        to_block: u64,
        event_signature: FixedBytes<32>,
    ) -> anyhow::Result<Vec<Log>> {
        let provider = self.get_provider(chain_id)?;

        let filter = Filter::new()
            .address(contract_address)
            .event_signature(event_signature)
            .from_block(from_block)
            .to_block(to_block);

        let logs = provider.get_logs(&filter).await?;
        Ok(logs)
    }

    /// Query logs in batches of `max_per_query`.
    pub async fn query_logs_batched(
        &self,
        chain_id: u64,
        contract_address: Address,
        from_block: u64,
        to_block: u64,
        event_signature: FixedBytes<32>,
        max_per_query: u64,
    ) -> anyhow::Result<Vec<Log>> {
        let mut all_logs = Vec::new();
        let mut current = from_block;

        while current < to_block {
            let batch_end = (current + max_per_query).min(to_block);
            let logs = self
                .query_logs(chain_id, contract_address, current, batch_end, event_signature)
                .await?;
            all_logs.extend(logs);
            current = batch_end + 1;
        }

        Ok(all_logs)
    }

    /// Parse a `HeaderRangeCommitmentStored` event log into a `DataCommitmentRange`.
    pub fn parse_data_commitment_log(log: &Log) -> anyhow::Result<DataCommitmentRange> {
        let decoded = log
            .log_decode::<HeaderRangeCommitmentStored>()
            .map_err(|e| anyhow::anyhow!("Failed to decode HeaderRangeCommitmentStored: {e}"))?;

        let inner = &decoded.inner.data;

        Ok(DataCommitmentRange {
            start_block_number: inner.startBlock,
            end_block_number: inner.endBlock,
            data_commitment: inner.dataCommitment.0,
            state_commitment: inner.stateCommitment.0,
            commitment_tree_size: inner.headerRangeCommitmentTreeSize,
        })
    }

    /// Find the `DataCommitmentRange` for a specific Avail block number by scanning contract logs.
    pub async fn get_data_commitment_range_for_block(
        &self,
        chain_id: u64,
        contract_address: Address,
        target_block: u32,
    ) -> anyhow::Result<Option<DataCommitmentRange>> {
        let provider = self.get_provider(chain_id)?;
        let latest_block = provider.get_block_number().await?;

        let batch_size: u64 = 10_000;
        let mut current_block = latest_block;
        let event_sig = HeaderRangeCommitmentStored::SIGNATURE_HASH;

        loop {
            let from = current_block.saturating_sub(batch_size);
            let logs = self
                .query_logs(chain_id, contract_address, from, current_block, event_sig)
                .await?;

            if !logs.is_empty() {
                let first = Self::parse_data_commitment_log(&logs[0])?;
                let last = Self::parse_data_commitment_log(&logs[logs.len() - 1])?;

                if target_block >= first.start_block_number + 1
                    && target_block <= last.end_block_number
                {
                    return Ok(Some(Self::binary_search_log(&logs, target_block)?));
                }
            } else {
                tracing::warn!("No ranges found for block {current_block}");
                return Ok(None);
            }

            if from == 0 {
                return Ok(None);
            }
            current_block = from;
        }
    }

    /// Binary search through sorted logs for the one containing `target_block`.
    fn binary_search_log(
        logs: &[Log],
        target_block: u32,
    ) -> anyhow::Result<DataCommitmentRange> {
        let mut left = 0;
        let mut right = logs.len() - 1;

        while left <= right {
            let mid = (left + right) / 2;
            let range = Self::parse_data_commitment_log(&logs[mid])?;

            if target_block >= range.start_block_number + 1
                && target_block <= range.end_block_number
            {
                return Ok(range);
            }

            if target_block < range.start_block_number + 1 {
                if mid == 0 {
                    break;
                }
                right = mid - 1;
            } else {
                left = mid + 1;
            }
        }

        anyhow::bail!("Log not found for target block {target_block}")
    }

    /// Get the block range covered by a VectorX contract.
    pub async fn get_block_range(
        &self,
        contract_address: Address,
        chain_id: u64,
    ) -> anyhow::Result<RangeInfo> {
        let provider = self.get_provider(chain_id)?;
        let latest_block = provider.get_block_number().await?;

        let hex_address = format!("0x{}", hex::encode(contract_address.as_slice()));
        let deployment = self
            .config
            .find_deployment(&hex_address, chain_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Deployment config not found for {hex_address} on chain {chain_id}"
                )
            })?;

        let batch_size: u64 = 10_000;
        let event_sig = HeaderRangeCommitmentStored::SIGNATURE_HASH;

        // Find the first data commitment log
        let mut cursor = deployment.cursor_start_block;
        let contract_range_start_block;
        loop {
            let logs = self
                .query_logs_batched(
                    chain_id,
                    contract_address,
                    cursor,
                    cursor + batch_size,
                    event_sig,
                    batch_size,
                )
                .await?;

            if logs.is_empty() {
                cursor += batch_size;
                if cursor > latest_block {
                    anyhow::bail!("No data commitment logs found");
                }
                continue;
            }

            let first = Self::parse_data_commitment_log(&logs[0])?;
            // +1 because the start block in Avail is one block ahead of the event start block
            contract_range_start_block = first.start_block_number + 1;
            break;
        }

        // Find the most recent data commitment log
        let mut cursor = latest_block;
        let contract_range_end_block;
        loop {
            let from = cursor.saturating_sub(batch_size);
            let logs = self
                .query_logs_batched(chain_id, contract_address, from, cursor, event_sig, batch_size)
                .await?;

            if logs.is_empty() {
                cursor = from;
                if cursor < contract_range_start_block as u64 {
                    anyhow::bail!("No data commitment logs found");
                }
                continue;
            }

            let last = Self::parse_data_commitment_log(&logs[logs.len() - 1])?;
            contract_range_end_block = last.end_block_number;
            break;
        }

        Ok(RangeInfo {
            start: contract_range_start_block,
            end: contract_range_end_block,
        })
    }

    /// Get the latest block number from the VectorX contract.
    #[allow(dead_code)]
    pub async fn get_latest_vector_block(
        &self,
        chain_id: u64,
        contract_address: Address,
    ) -> anyhow::Result<u32> {
        let provider = self.get_provider(chain_id)?;
        let contract = VectorX::VectorXInstance::new(contract_address, provider.clone());
        let latest = contract.latestBlock().call().await?;
        Ok(latest)
    }

    /// Get the health status for a VectorX contract.
    ///
    /// Fetches the latest vector block and log activity in parallel where possible,
    /// reusing the single provider instance for this chain.
    pub async fn get_health_status(
        &self,
        contract_address: Address,
        chain_id: u64,
        avail_head_block: u64,
        max_delay_seconds: u64,
    ) -> anyhow::Result<HealthInfo> {
        let provider = self.get_provider(chain_id)?;

        // 1. Fetch latest vector block and current ETH block in parallel
        let contract = VectorX::VectorXInstance::new(contract_address, provider.clone());
        let latest_block_call = contract.latestBlock();
        let latest_block_fut = latest_block_call.call();
        let current_block_fut =
            provider.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest);

        let (latest_vector_result, current_block_result) =
            tokio::join!(latest_block_fut, current_block_fut);

        let latest_vector_block = latest_vector_result?;
        let current_block = current_block_result?
            .ok_or_else(|| anyhow::anyhow!("Failed to get latest block"))?;

        let current_block_number = current_block.header.number;
        let current_block_timestamp = current_block.header.timestamp;

        // 2. Estimate the block from `max_delay_seconds` ago using block time heuristic
        //    instead of the expensive binary search over RPC.
        //    Sepolia ~12s/block, mainnet ~12s/block.
        let estimated_blocks_back = max_delay_seconds / 12;
        let search_from = current_block_number.saturating_sub(estimated_blocks_back * 10);

        let event_sig = HeadUpdate::SIGNATURE_HASH;
        let logs = self
            .query_logs_batched(
                chain_id,
                contract_address,
                search_from,
                current_block_number,
                event_sig,
                10_000,
            )
            .await?;

        let log_emitted = !logs.is_empty();
        let last_log_block_number = if logs.is_empty() {
            search_from
        } else {
            logs.iter()
                .filter_map(|l| l.block_number)
                .max()
                .unwrap_or(search_from)
        };

        let eth_blocks_since_last_log = current_block_number.saturating_sub(last_log_block_number);

        // 3. Get the timestamp of the last log block
        let last_log_timestamp = if last_log_block_number == current_block_number {
            current_block_timestamp
        } else {
            let last_log_block = provider
                .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(last_log_block_number))
                .await?
                .ok_or_else(|| anyhow::anyhow!("Failed to get last log block"))?;
            last_log_block.header.timestamp
        };

        Ok(HealthInfo {
            blocks_behind_head: avail_head_block as i64 - latest_vector_block as i64,
            eth_blocks_since_last_log,
            last_log_timestamp,
            log_emitted,
        })
    }
}

/// Compute the keccak256 range hash for (startBlock, endBlock) encoded as ABI parameters.
pub fn get_range_hash(start_block: u32, end_block: u32) -> [u8; 32] {
    use tiny_keccak::{Hasher, Keccak};

    // ABI encode: uint32 start, uint32 end (each padded to 32 bytes)
    let mut encoded = [0u8; 64];
    let start_u256 = U256::from(start_block);
    let end_u256 = U256::from(end_block);
    encoded[..32].copy_from_slice(&start_u256.to_be_bytes::<32>());
    encoded[32..].copy_from_slice(&end_u256.to_be_bytes::<32>());

    let mut output = [0u8; 32];
    let mut hasher = Keccak::v256();
    hasher.update(&encoded);
    hasher.finalize(&mut output);
    output
}
