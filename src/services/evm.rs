use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::sync::Arc;

use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::types::{Filter, Log};
use alloy::sol;
use alloy::sol_types::SolEvent;

use crate::config::{AppConfig, DeploymentEntry};

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
    /// EVM blocks from the log window of the first commitment to the window of the newest one.
    #[serde(skip)]
    pub log_blocks: RangeInclusive<u64>,
    /// The newest commitment.
    #[serde(skip)]
    pub newest: DataCommitmentRange,
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

    fn get_deployment(
        &self,
        contract_address: Address,
        chain_id: u64,
    ) -> anyhow::Result<&DeploymentEntry> {
        let hex_address = format!("0x{}", hex::encode(contract_address.as_slice()));
        self.config
            .find_deployment(&hex_address, chain_id)
            .ok_or_else(|| {
                anyhow::anyhow!("Deployment config not found for {hex_address} on chain {chain_id}")
            })
    }

    /// Query event logs from the contract.
    ///
    /// If `to_block` lands beyond the serving node's head (common with load-balanced
    /// RPCs whose `eth_getLogs` backend lags `eth_blockNumber`), retry against the
    /// node's own `latest` head instead of failing.
    pub async fn query_logs(
        &self,
        chain_id: u64,
        contract_address: Address,
        from_block: u64,
        to_block: u64,
        event_signature: FixedBytes<32>,
    ) -> anyhow::Result<Vec<Log>> {
        let provider = self.get_provider(chain_id)?;

        let base = Filter::new()
            .address(contract_address)
            .event_signature(event_signature)
            .from_block(from_block);

        match provider.get_logs(&base.clone().to_block(to_block)).await {
            Ok(logs) => Ok(logs),
            Err(e) if e.to_string().contains("beyond current head") => {
                let filter = base.to_block(alloy::eips::BlockNumberOrTag::Latest);
                let logs = provider.get_logs(&filter).await?;
                Ok(logs)
            }
            Err(e) => Err(e.into()),
        }
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
                .query_logs(
                    chain_id,
                    contract_address,
                    current,
                    batch_end,
                    event_signature,
                )
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

    /// Find the `DataCommitmentRange` for a specific Avail block number.
    ///
    /// Commitment logs hold consecutive Avail ranges in EVM block order, so a binary search over
    /// the log blocks of `range`, which comes from `get_block_range`, finds the right log.
    pub async fn get_data_commitment_range_for_block(
        &self,
        chain_id: u64,
        contract_address: Address,
        target_block: u32,
        range: &RangeInfo,
    ) -> anyhow::Result<Option<DataCommitmentRange>> {
        let newest = &range.newest;
        if target_block > newest.start_block_number && target_block <= newest.end_block_number {
            return Ok(Some(newest.clone()));
        }

        // The log for `target_block` lies in the EVM blocks `low..high`.
        let mut low = *range.log_blocks.start();
        let mut high = *range.log_blocks.end() + 1;
        while low < high {
            let mid = low + (high - low) / 2;
            let Some((logs, window_end)) = self
                .first_commitment_logs(chain_id, contract_address, mid, high - 1)
                .await?
            else {
                high = mid;
                continue;
            };

            let first = Self::parse_data_commitment_log(&logs[0])?;
            let last = Self::parse_data_commitment_log(&logs[logs.len() - 1])?;
            if target_block < first.start_block_number + 1 {
                high = mid;
            } else if target_block > last.end_block_number {
                low = window_end + 1;
            } else {
                return Ok(Some(Self::binary_search_log(&logs, target_block)?));
            }
        }

        tracing::warn!(target_block, "No data commitment range found");
        Ok(None)
    }

    /// Return the logs of the first log window in `from..=to` that holds a commitment,
    /// together with the last block of that window.
    async fn first_commitment_logs(
        &self,
        chain_id: u64,
        contract_address: Address,
        mut from: u64,
        to: u64,
    ) -> anyhow::Result<Option<(Vec<Log>, u64)>> {
        let batch_size: u64 = 10_000;
        let event_sig = HeaderRangeCommitmentStored::SIGNATURE_HASH;

        while from <= to {
            let window_end = (from + batch_size - 1).min(to);
            let logs = self
                .query_logs(chain_id, contract_address, from, window_end, event_sig)
                .await?;
            if !logs.is_empty() {
                return Ok(Some((logs, window_end)));
            }
            from = window_end + 1;
        }
        Ok(None)
    }

    /// Binary search through sorted logs for the one containing `target_block`.
    fn binary_search_log(logs: &[Log], target_block: u32) -> anyhow::Result<DataCommitmentRange> {
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
        let deployment = self.get_deployment(contract_address, chain_id)?;
        let provider = self.get_provider(chain_id)?;
        let latest_block = provider.get_block_number().await?;

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
        let first_log_window = cursor;

        // Find the most recent data commitment log
        let mut cursor = latest_block;
        let newest;
        loop {
            let from = cursor.saturating_sub(batch_size);
            let logs = self
                .query_logs_batched(
                    chain_id,
                    contract_address,
                    from,
                    cursor,
                    event_sig,
                    batch_size,
                )
                .await?;

            if logs.is_empty() {
                cursor = from;
                if cursor <= deployment.cursor_start_block {
                    anyhow::bail!("No data commitment logs found");
                }
                continue;
            }

            newest = Self::parse_data_commitment_log(&logs[logs.len() - 1])?;
            break;
        }

        Ok(RangeInfo {
            start: contract_range_start_block,
            end: newest.end_block_number,
            log_blocks: first_log_window..=cursor,
            newest,
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
        let current_block_fut = provider.get_block_by_number(alloy::eips::BlockNumberOrTag::Latest);

        let (latest_vector_result, current_block_result) =
            tokio::join!(latest_block_fut, current_block_fut);

        let latest_vector_block = latest_vector_result?;
        let current_block =
            current_block_result?.ok_or_else(|| anyhow::anyhow!("Failed to get latest block"))?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeploymentEntry;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use alloy::primitives::U64;
    use alloy::rpc::client::RpcClient;
    use alloy::rpc::json_rpc::{RequestPacket, Response, ResponsePacket, ResponsePayload};
    use alloy::transports::mock::Asserter;
    use alloy::transports::{TransportError, TransportFut};
    use tower::Service;

    const CHAIN_ID: u64 = 421614;
    const CONTRACT: Address = Address::repeat_byte(0x11);

    /// Config with a single Turing deployment of `CONTRACT` on `CHAIN_ID`.
    fn test_config(cursor_start_block: u64) -> AppConfig {
        AppConfig {
            rpc_urls: HashMap::new(),
            avail_rpc_url: String::new(),
            avail_network: "turing".to_string(),
            database_url: String::new(),
            server_host: String::new(),
            server_port: 0,
            deployments: vec![DeploymentEntry {
                contract_chain_id: CHAIN_ID,
                contract_address: CONTRACT.to_string(),
                cursor_start_block,
                avail_chain_id: "turing".to_string(),
            }],
        }
    }

    /// Build a service whose only provider answers RPC calls from `asserter`, in queue order.
    fn mocked_service(cursor_start_block: u64, asserter: &Asserter) -> EvmService {
        let provider = RootProvider::new(RpcClient::mocked(asserter.clone()));
        EvmService {
            config: Arc::new(test_config(cursor_start_block)),
            providers: Arc::new(HashMap::from([(CHAIN_ID, provider)])),
        }
    }

    fn commitment_log(start_block: u32, end_block: u32) -> Log {
        let event = HeaderRangeCommitmentStored {
            startBlock: start_block,
            endBlock: end_block,
            dataCommitment: FixedBytes::ZERO,
            stateCommitment: FixedBytes::ZERO,
            headerRangeCommitmentTreeSize: 512,
        };
        Log {
            inner: alloy::primitives::Log {
                address: CONTRACT,
                data: event.encode_log_data(),
            },
            ..Default::default()
        }
    }

    fn commitment_log_at(evm_block: u64, start_block: u32, end_block: u32) -> Log {
        Log {
            block_number: Some(evm_block),
            ..commitment_log(start_block, end_block)
        }
    }

    /// Chain on which the contract emitted one commitment log per `(evm_block, start, end)`.
    /// It answers `eth_blockNumber` and `eth_getLogs` by block range, and records the queries.
    #[derive(Clone)]
    struct FakeChain {
        head: u64,
        commitments: Arc<Vec<(u64, u32, u32)>>,
        log_queries: Arc<AtomicUsize>,
        lowest_from_block: Arc<AtomicU64>,
    }

    impl FakeChain {
        fn new(head: u64, commitments: Vec<(u64, u32, u32)>) -> Self {
            Self {
                head,
                commitments: Arc::new(commitments),
                log_queries: Arc::new(AtomicUsize::new(0)),
                lowest_from_block: Arc::new(AtomicU64::new(u64::MAX)),
            }
        }

        fn service(&self, cursor_start_block: u64) -> EvmService {
            let provider = RootProvider::new(RpcClient::new(self.clone(), true));
            EvmService {
                config: Arc::new(test_config(cursor_start_block)),
                providers: Arc::new(HashMap::from([(CHAIN_ID, provider)])),
            }
        }

        fn logs_between(&self, from: u64, to: u64) -> Vec<Log> {
            self.commitments
                .iter()
                .filter(|(block, ..)| (from..=to).contains(block))
                .map(|&(block, start, end)| commitment_log_at(block, start, end))
                .collect()
        }

        /// Look up `target_block` the way the proof route does, with the contract range first.
        /// Returns the range found and the number of log queries the lookup itself made.
        async fn lookup(
            &self,
            cursor_start_block: u64,
            target_block: u32,
        ) -> (Option<DataCommitmentRange>, usize) {
            let service = self.service(cursor_start_block);
            let range = service.get_block_range(CONTRACT, CHAIN_ID).await.unwrap();
            let before = self.log_queries.load(Ordering::Relaxed);
            let found = service
                .get_data_commitment_range_for_block(CHAIN_ID, CONTRACT, target_block, &range)
                .await
                .unwrap();
            (found, self.log_queries.load(Ordering::Relaxed) - before)
        }
    }

    impl Service<RequestPacket> for FakeChain {
        type Response = ResponsePacket;
        type Error = TransportError;
        type Future = TransportFut<'static>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: RequestPacket) -> Self::Future {
            let RequestPacket::Single(req) = req else {
                panic!("batch requests are not used");
            };
            let result = match req.method() {
                "eth_blockNumber" => serde_json::to_value(U64::from(self.head)).unwrap(),
                "eth_getLogs" => {
                    let params: serde_json::Value =
                        serde_json::from_str(req.params().unwrap().get()).unwrap();
                    let filter = &params[0];
                    assert_eq!(filter["address"], serde_json::to_value(CONTRACT).unwrap());
                    assert_eq!(
                        filter["topics"][0],
                        serde_json::to_value(HeaderRangeCommitmentStored::SIGNATURE_HASH).unwrap()
                    );
                    let block = |key: &str| {
                        let hex = filter[key].as_str().unwrap().trim_start_matches("0x");
                        u64::from_str_radix(hex, 16).unwrap()
                    };
                    let (from, to) = (block("fromBlock"), block("toBlock"));
                    assert!(from <= to && to <= self.head, "log query {from}..={to}");
                    assert!(to - from <= 10_000, "log query {from}..={to}");
                    self.log_queries.fetch_add(1, Ordering::Relaxed);
                    self.lowest_from_block.fetch_min(from, Ordering::Relaxed);
                    serde_json::to_value(self.logs_between(from, to)).unwrap()
                }
                method => panic!("unexpected RPC method {method}"),
            };
            let response = Response {
                id: req.id().clone(),
                payload: ResponsePayload::Success(
                    serde_json::value::to_raw_value(&result).unwrap(),
                ),
            };
            Box::pin(async move { Ok(ResponsePacket::Single(response)) })
        }
    }

    /// `count` consecutive ranges of 360 Avail blocks, one every `spacing` EVM blocks.
    fn regular_commitments(first_block: u64, spacing: u64, count: u32) -> Vec<(u64, u32, u32)> {
        (0..count)
            .map(|i| {
                let start = 1_000 + i * 360;
                (first_block + u64::from(i) * spacing, start, start + 360)
            })
            .collect()
    }

    #[tokio::test]
    async fn test_lookup_finds_oldest_range_in_long_history() {
        // About 27,000 log windows between the deployment start and the head.
        let start = 59_000_000;
        let chain = FakeChain::new(
            start + 270_000_000,
            regular_commitments(start + 5_000, 25_000, 10_800),
        );

        let (range, queries) = chain.lookup(start, 1_001).await;
        let range = range.expect("the oldest range must be found");
        assert_eq!(
            (range.start_block_number, range.end_block_number),
            (1_000, 1_360)
        );
        assert!(queries < 100, "{queries} log queries");
    }

    #[tokio::test]
    async fn test_lookup_returns_newest_range_without_log_queries() {
        let start = 59_000_000;
        let chain = FakeChain::new(
            start + 270_000_000,
            regular_commitments(start + 5_000, 25_000, 10_800),
        );
        let newest_end = 1_000 + 10_800 * 360;

        let (range, queries) = chain.lookup(start, newest_end).await;
        let range = range.expect("the newest range must be found");
        assert_eq!(
            (range.start_block_number, range.end_block_number),
            (newest_end - 360, newest_end)
        );
        assert_eq!(queries, 0);
    }

    #[tokio::test]
    async fn test_lookup_skips_blocks_after_newest_commitment() {
        // Commitments stopped after about 1,000 windows, and the head is 25,000 windows later.
        let start = 59_000_000;
        let chain = FakeChain::new(
            start + 260_000_000,
            regular_commitments(start + 5_000, 10_000, 1_000),
        );

        let (range, queries) = chain.lookup(start, 1_001).await;
        let range = range.expect("the oldest range must be found");
        assert_eq!(
            (range.start_block_number, range.end_block_number),
            (1_000, 1_360)
        );
        assert!(queries < 50, "{queries} log queries");
    }

    #[tokio::test]
    async fn test_lookup_finds_every_range_across_gaps() {
        // Logs at both ends of the searched blocks, in adjacent blocks, and after 45 empty windows.
        let blocks = [
            1_000_500, 1_000_900, 1_020_000, 1_150_000, 1_150_001, 1_600_000, 1_610_000,
        ];
        let commitments: Vec<_> = blocks
            .iter()
            .zip(0u32..)
            .map(|(&block, i)| (block, 100 + i * 50, 150 + i * 50))
            .collect();
        let chain = FakeChain::new(1_610_000, commitments.clone());

        for &(_, start, end) in &commitments {
            for target in [start + 1, end] {
                let (range, _) = chain.lookup(1_000_500, target).await;
                let range = range.unwrap_or_else(|| panic!("no range for block {target}"));
                assert_eq!(
                    (range.start_block_number, range.end_block_number),
                    (start, end),
                    "block {target}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_lookup_returns_none_for_uncommitted_block() {
        let start = 59_000_000;
        let chain = FakeChain::new(
            start + 270_000_000,
            regular_commitments(start + 5_000, 25_000, 10_800),
        );
        let newest_end = 1_000 + 10_800 * 360;

        let (range, queries) = chain.lookup(start, newest_end + 1).await;
        assert!(range.is_none());
        assert!(queries < 100, "{queries} log queries");
    }

    #[tokio::test]
    async fn test_lookup_stays_above_deployment_start() {
        let chain = FakeChain::new(
            1_700_000,
            vec![(1_050_000, 100, 150), (1_060_000, 150, 200)],
        );

        let (range, _) = chain.lookup(1_005_000, 50).await;
        assert!(range.is_none());
        assert!(chain.lowest_from_block.load(Ordering::Relaxed) >= 1_005_000);
    }

    #[tokio::test]
    async fn test_range_response_has_only_start_and_end() {
        let chain = FakeChain::new(
            1_700_000,
            vec![(1_050_000, 100, 150), (1_060_000, 150, 200)],
        );

        let range = chain
            .service(1_000_000)
            .get_block_range(CONTRACT, CHAIN_ID)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&range).unwrap(),
            serde_json::json!({ "start": 101, "end": 200 })
        );
    }

    #[tokio::test]
    async fn test_block_range_scan_compares_evm_blocks() {
        // The Avail start block (50_001) is above the EVM blocks scanned,
        // so the backward scan must not compare against it.
        let asserter = Asserter::new();
        asserter.push_success(&U64::from(30_000));
        asserter.push_success(&vec![commitment_log(50_000, 50_100)]); // [1_000, 11_000]
        asserter.push_success(&Vec::<Log>::new()); // [20_000, 30_000]
        asserter.push_success(&vec![commitment_log(50_000, 50_100)]); // [10_000, 20_000]
        let service = mocked_service(1_000, &asserter);

        let range = service.get_block_range(CONTRACT, CHAIN_ID).await.unwrap();
        assert_eq!((range.start, range.end), (50_001, 50_100));
    }
}
