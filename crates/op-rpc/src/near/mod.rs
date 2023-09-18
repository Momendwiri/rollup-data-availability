use std::str::FromStr;

use super::{Blob, DataAvailability};
use crate::{Commitment, IndexRead, Read, ReadAll, SubmitResult};
use config::Config;
use eyre::{eyre, Result};
use futures::TryFutureExt;
use log::{debug, error};
use near_crypto::{InMemorySigner, Signer};
use near_da_primitives::Namespace;
use near_jsonrpc_client::{
    methods::{self, broadcast_tx_commit::RpcBroadcastTxCommitRequest, query::RpcQueryRequest},
    JsonRpcClient,
};
use near_jsonrpc_primitives::types::query::QueryResponseKind;
use near_primitives::{
    hash::CryptoHash,
    transaction::{Action, FunctionCallAction, Transaction},
    types::{AccountId, BlockHeight, BlockId, BlockReference, FunctionArgs, Nonce},
    views::QueryRequest::CallFunction,
};
use serde_json::{json, Value};

pub mod config;

pub const MAX_TGAS: u64 = 300_000_000_000_000;

pub struct Client {
    pub config: Config,
    pub client: JsonRpcClient,
    pub archive: JsonRpcClient,
}

impl Client {
    pub fn new(config: &Config) -> Self {
        Self {
            config: config.clone(),
            client: JsonRpcClient::connect(config.network.to_endpoint()),
            archive: JsonRpcClient::connect(config.network.archive_endpoint()),
        }
    }

    async fn get_current_nonce(
        &self,
        account_id: &AccountId,
        public_key: &near_crypto::PublicKey,
    ) -> Result<Option<(CryptoHash, Nonce)>> {
        let query_response = self
            .client
            .call(RpcQueryRequest {
                block_reference: BlockReference::latest(),
                request: near_primitives::views::QueryRequest::ViewAccessKey {
                    account_id: account_id.clone(),
                    public_key: public_key.clone(),
                },
            })
            .await;

        match query_response {
            Ok(access_key_query_response) => match access_key_query_response.kind {
                QueryResponseKind::AccessKey(access_key) => Ok(Some((
                    access_key_query_response.block_hash,
                    access_key.nonce,
                ))),
                _ => Err(eyre!("failed to extract current nonce")),
            },
            Err(res) => Err(res)?,
        }
    }

    pub async fn get_nonce_signer(&self) -> Result<(InMemorySigner, CryptoHash, Nonce)> {
        let signer = get_signer(&self.config)?;
        if let Some((latest_hash, current_nonce)) = self
            .get_current_nonce(&signer.account_id, &signer.public_key)
            .await?
        {
            Ok((signer, latest_hash, current_nonce))
        } else {
            Err(eyre!("failed to get current nonce"))
        }
    }

    pub async fn no_signer(&self) -> Result<impl Signer> {
        Ok(near_crypto::EmptySigner {})
    }

    pub fn build_view_call(
        contract: &AccountId,
        height: Option<BlockHeight>,
        method: &str,
        json: Value,
    ) -> RpcQueryRequest {
        RpcQueryRequest {
            block_reference: height
                .map(|height| BlockReference::BlockId(BlockId::Height(height)))
                .unwrap_or(BlockReference::latest()),
            request: CallFunction {
                account_id: contract.clone(),
                method_name: method.to_string(),
                args: FunctionArgs::from(json.to_string().into_bytes()),
            },
        }
    }

    pub fn build_function_call_transaction<S: Signer>(
        signer: &S,
        signer_account_id: &AccountId,
        contract: &AccountId,
        latest_hash: &CryptoHash,
        current_nonce: Nonce,
        action: FunctionCallAction,
    ) -> RpcBroadcastTxCommitRequest {
        let tx = Transaction {
            signer_id: signer_account_id.clone(),
            public_key: signer.public_key(),
            nonce: current_nonce + 1,
            receiver_id: contract.clone(),
            block_hash: *latest_hash,
            actions: vec![Action::FunctionCall(action)],
        };
        methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest {
            signed_transaction: tx.sign(signer),
        }
    }
}

pub fn get_signer(config: &Config) -> Result<InMemorySigner> {
    Ok(match config.key {
        config::KeyType::File(ref path) => InMemorySigner::from_file(path)?,
        config::KeyType::Seed(ref account_id, ref seed) => {
            InMemorySigner::from_seed(account_id.parse()?, near_crypto::KeyType::ED25519, seed)
        }
        config::KeyType::SecretKey(ref account_id, ref secret_key) => {
            InMemorySigner::from_secret_key(
                account_id.parse()?,
                near_crypto::SecretKey::from_str(&secret_key)?,
            )
        }
    })
}

// TODO: mock tests for these
#[async_trait::async_trait]
impl DataAvailability for Client {
    async fn submit(&self, blobs: &[Blob]) -> Result<SubmitResult> {
        let (signer, latest_hash, current_nonce) = self.get_nonce_signer().await?;

        let req = Client::build_function_call_transaction(
            &signer,
            &signer.account_id.parse()?,
            &self.config.contract.parse()?,
            &latest_hash,
            current_nonce,
            FunctionCallAction {
                method_name: "submit".to_string(),
                args: json!({
                    "blobs": blobs,
                })
                .to_string()
                .into_bytes(),
                gas: MAX_TGAS,
                deposit: 0,
            },
        );
        let result = self
            .client
            .call(&req)
            .or_else(|e| {
                debug!("Error hitting main rpc, falling back to archive: {:?}", e);
                self.archive.call(&req)
            })
            .await?;

        match result.status {
            near_primitives::views::FinalExecutionStatus::Failure(err) => {
                error!("Error submitting transaction: {:?}", err);
                Err(eyre!("Error submitting transaction: {:?}", err))
            }
            near_primitives::views::FinalExecutionStatus::SuccessValue(bytes) => {
                debug!("Transaction submitted: {:?}", bytes);
                let height: BlockHeight = serde_json::from_slice(&bytes)?;
                Ok(SubmitResult(height))
            }
            x => Err(eyre!("Transaction not ready yet: {:?}", x)),
        }
    }

    async fn get(&self, namespace: &Namespace, height: BlockHeight) -> Result<Read> {
        let req = Client::build_view_call(
            &self.config.contract.parse()?,
            None,
            "get",
            json!({
                "namespace": namespace,
                "height": height,
            }),
        );
        let result = self
            .client
            .call(&req)
            .or_else(|e| {
                debug!("Error hitting main rpc, falling back to archive: {:?}", e);
                self.archive.call(&req)
            })
            .await?;

        if let QueryResponseKind::CallResult(call_result) = result.kind {
            let blob: Option<Blob> = serde_json::from_slice(&call_result.result)?;
            debug!("Got blob: {:?}", blob);
            blob.map(Read).ok_or_else(|| eyre!("Blob not found"))
        } else {
            Err(eyre!("Transaction not ready yet: {:?}", result))
        }
    }

    async fn get_all(&self, namespace: &Namespace) -> Result<ReadAll> {
        let req = Client::build_view_call(
            &self.config.contract.parse()?,
            None,
            "get_all",
            json!({
                "namespace": namespace,
            }),
        );
        let result = self
            .client
            .call(&req)
            .or_else(|e| {
                debug!("Error hitting main rpc, falling back to archive: {:?}", e);
                self.archive.call(&req)
            })
            .await?;

        if let QueryResponseKind::CallResult(call_result) = result.kind {
            let blobs: Vec<(BlockHeight, Blob)> = serde_json::from_slice(&call_result.result)?;
            debug!("Got blobs: {:?}", blobs);
            Ok(ReadAll(blobs))
        } else {
            Err(eyre!("Transaction not ready yet: {:?}", result))
        }
    }

    async fn fast_get(&self, commitment: &Commitment) -> Result<IndexRead> {
        let req = Client::build_view_call(
            &self.config.contract.parse()?,
            None,
            "fast_get",
            json!({
                "commitment": commitment,
            }),
        );
        let result = self
            .client
            .call(&req)
            .or_else(|e| {
                debug!("Error hitting main rpc, falling back to archive: {:?}", e);
                self.archive.call(&req)
            })
            .await?;

        if let QueryResponseKind::CallResult(call_result) = result.kind {
            let blob: Option<Blob> = serde_json::from_slice(&call_result.result)?;
            debug!("Got blob: {:?}", blob);
            blob.map(IndexRead).ok_or_else(|| eyre!("Blob not found"))
        } else {
            Err(eyre!("Transaction not ready yet: {:?}", result))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_signer() {
        let signer = get_signer(&Config {
            key: config::KeyType::File("throwaway-key.json".to_string().into()),
            ..Default::default()
        })
        .unwrap();
        let account_id = "throwawaykey.testnet";
        let public_key = "ed25519:BLpBXcR5eNg43nDdV3Vkk5UQTC2yaz3x1v9oJMRminMg";

        assert_eq!(signer.account_id.to_string(), account_id.to_string());
        assert_eq!(signer.public_key.to_string(), public_key.to_string());

        let signer = get_signer(&Config {
            key: config::KeyType::Seed(account_id.parse().unwrap(), "ed25519:test".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(signer.account_id.to_string(), account_id.to_string());
        assert_eq!(
            signer.public_key.to_string(),
            "ed25519:38FBJoAPGsefiNoTFoDr95zyGeMb6fx6MuQw9HaasxHH".to_string()
        );

        let signer = get_signer(&Config {
            key: config::KeyType::SecretKey(
                account_id.parse().unwrap(),
                "ed25519:38FBJoAPGsefiNoTFoDr95zyGeMb6fx6MuQw9HaasxHH38FBJoAPGsefiNoTFoDr95zyGeMb6fx6MuQw9HaasxHH".to_string(),
            ),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(signer.account_id.to_string(), account_id.to_string());
        assert_eq!(signer.public_key.to_string(), "ed25519:6m6vtRuWa59EaqrY5txxtK6te2KdJy3zna74MWfEETG7".to_string());
    }
    #[test]
    fn t() {}

    #[test]
    fn test_build_fast_get() {}

    #[test]
    fn test_build_get_all() {}

    #[test]
    fn test_build_get() {}

    #[test]
    fn test_build_submit() {}
}
