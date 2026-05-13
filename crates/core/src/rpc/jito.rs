use std::sync::Arc;

use jsonrpc_core::{BoxFuture, Error, Result};
use jsonrpc_derive::rpc;
use sha2::{Digest, Sha256};
use solana_client::{rpc_config::RpcSendTransactionConfig, rpc_custom_error::RpcCustomError};
use solana_rpc_client_api::response::{Response as RpcResponse, RpcResponseContext};
use solana_signature::Signature;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::{TransactionConfirmationStatus, UiTransactionEncoding};
use surfpool_types::{JitoBundleStatus, TransactionStatusEvent};

use super::{RunloopContext, utils::decode_and_deserialize};
use crate::{
    rpc::full::SurfpoolFullRpc,
    surfnet::{locker::SurfnetSvmLocker, svm::BundleSandbox},
};

/// Maximum number of transactions allowed in a single bundle, matching Jito's limit.
const MAX_BUNDLE_SIZE: usize = 5;

/// Maximum number of bundle IDs accepted in a single `getBundleStatuses` request, matching
/// Jito's documented limit. Larger batches are rejected with `invalid_params`.
const MAX_BUNDLES_PER_QUERY: usize = 5;

/// Jito-specific RPC methods for bundle submission
#[rpc]
pub trait Jito {
    type Metadata;

    /// Sends a bundle of transactions to be processed atomically.
    ///
    /// This RPC method accepts a bundle of transactions (Jito-compatible format) and processes them
    /// one by one in order against an isolated sandbox VM. **The bundle is all-or-nothing**: if any
    /// transaction in the bundle fails (simulation error, execution error, or verification error),
    /// every other transaction's effects are discarded and the underlying VM is left byte-identical
    /// to its pre-bundle state. No Geyser event, no Simnet event, and no WebSocket subscriber
    /// notification is dispatched for a bundle that fails.
    ///
    /// On full success, the sandbox's state changes — account mutations, transaction storage
    /// writes, token-account index updates, write-version increments, etc. — are atomically
    /// committed onto the original VM under an exclusive writer guard, and Geyser/Simnet events
    /// plus WebSocket subscriber notifications (account, program, signature, logs) are fired
    /// onto the live event channels exactly as if each transaction had been submitted through
    /// the regular `sendTransaction` RPC.
    ///
    /// ## Parameters
    /// - `transactions`: An array of serialized transaction data (base64 or base58 encoded).
    /// - `config`: Optional configuration for encoding format.
    ///
    /// ## Returns
    /// - `BoxFuture<Result<String>>`: A future resolving to the bundle ID (SHA-256 hash of
    ///   comma-separated signatures), or an error if any transaction in the bundle fails.
    ///   Returning a future (rather than blocking) lets the JSON-RPC runtime drive the async
    ///   sandbox execution without spawning a nested tokio runtime on an HTTP worker thread.
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "sendBundle",
    ///   "params": [
    ///     ["base64EncodedTx1", "base64EncodedTx2"],
    ///     { "encoding": "base64" }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Notes
    /// - Bundles are limited to a maximum of 5 transactions, matching Jito's limit.
    /// - Transactions are processed sequentially in the order provided against a single sandbox.
    /// - Atomicity is guaranteed: on any failure the original VM is unaffected.
    /// - The bundle ID is calculated as SHA-256 hash of comma-separated transaction signatures.
    #[rpc(meta, name = "sendBundle")]
    fn send_bundle(
        &self,
        meta: Self::Metadata,
        transactions: Vec<String>,
        config: Option<RpcSendTransactionConfig>,
    ) -> BoxFuture<Result<String>>;

    /// Retrieves Jito-shaped status summaries for one or more previously submitted bundles.
    ///
    /// Mirrors Jito's wire protocol: the first positional parameter is an **array** of bundle IDs
    /// (max [`MAX_BUNDLES_PER_QUERY`]). For each requested id we resolve per-signature status via
    /// the same path as `getSignatureStatuses` and return a single aggregate status object per
    /// bundle, or `null` at that index when the bundle is unknown locally.
    ///
    /// ## Parameters
    /// - `bundle_ids`: Array of bundle identifiers returned by `sendBundle`. May contain up to
    ///   [`MAX_BUNDLES_PER_QUERY`] entries; the empty array is rejected with `invalid_params`.
    ///
    /// ## Returns
    /// On success, the JSON-RPC `result` is the standard Solana contextualized response shape:
    /// - `context.slot`: Context slot from the underlying status query (same idea as `getSignatureStatuses`).
    /// - `value`: An array with exactly **one element per input bundle id**, at the same index:
    ///   - `null` when the bundle id is not known locally (no stored signatures — the id may be
    ///     valid elsewhere, we simply have nothing to report).
    ///   - A `surfpool_types::JitoBundleStatus` object otherwise, with:
    ///     - `bundle_id`: The requested bundle id (snake_case wire field).
    ///     - `transactions`: Base-58 signatures in bundle submission order (from local `jito_bundles` storage).
    ///     - `slot`: Slot from the first per-signature status entry (bundle txs share a landing slot), or `0` if none yet.
    ///     - `confirmation_status`: From that same first entry (defaults to `processed` when absent).
    ///     - `err`: `Ok` if no transaction error was observed on any status; otherwise the first `Err` encountered
    ///       (JSON-serialized like other Solana `Result` values, e.g. `{"Ok": null}` or `{"Err": ...}`).
    ///
    /// ## Example Request (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "method": "getBundleStatuses",
    ///   "params": [
    ///     ["bundleIdHere", "anotherBundleId"]
    ///   ]
    /// }
    /// ```
    ///
    /// ## Example Response (JSON-RPC)
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "id": 1,
    ///   "result": {
    ///     "context": { "slot": 242806119 },
    ///     "value": [
    ///       {
    ///         "bundle_id": "892b79ed49138bfb3aa5441f0df6e06ef34f9ee8f3976c15b323605bae0cf51d",
    ///         "transactions": [
    ///           "3bC2M9fiACSjkTXZDgeNAuQ4ScTsdKGwR42ytFdhUvikqTmBheUxfsR1fDVsM5ADCMMspuwGkdm1uKbU246x5aE3",
    ///           "8t9hKYEYNbLvNqiSzP96S13XF1C2f1ro271Kdf7bkZ6EpjPLuDff1ywRy4gfaGSTubsM2FeYGDoT64ZwPm1cQUt"
    ///         ],
    ///         "slot": 242804011,
    ///         "confirmation_status": "finalized",
    ///         "err": { "Ok": null }
    ///       },
    ///       null
    ///     ]
    ///   }
    /// }
    /// ```
    ///
    /// ## Notes
    /// - Bundles are stored locally as a mapping from `bundle_id` to a list of base-58 signatures.
    /// - Unknown bundle ids appear as `null` **inside** the `value` array; the outer `result` is
    ///   never `null` (Jito-style: per-index reporting).
    /// - Per-signature status resolution uses the same logic as `getSignatureStatuses` (local store and optional remote datasource).
    #[rpc(meta, name = "getBundleStatuses")]
    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_ids: Vec<String>,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<JitoBundleStatus>>>>>;
}

#[derive(Clone)]
pub struct SurfpoolJitoRpc;

impl Jito for SurfpoolJitoRpc {
    type Metadata = Option<RunloopContext>;

    fn send_bundle(
        &self,
        meta: Self::Metadata,
        transactions: Vec<String>,
        config: Option<RpcSendTransactionConfig>,
    ) -> BoxFuture<Result<String>> {
        Box::pin(async move {
            if transactions.is_empty() {
                return Err(Error::invalid_params("Bundle cannot be empty"));
            }

            if transactions.len() > MAX_BUNDLE_SIZE {
                return Err(Error::invalid_params(format!(
                    "Bundle exceeds maximum size of {MAX_BUNDLE_SIZE} transactions"
                )));
            }

            let Some(ctx) = meta else {
                return Err(RpcCustomError::NodeUnhealthy {
                    num_slots_behind: None,
                }
                .into());
            };

            let base_config = config.unwrap_or_default();

            // Decode all bundle transactions up front so we can run them against an isolated
            // sandbox.
            let tx_encoding = base_config
                .encoding
                .unwrap_or(UiTransactionEncoding::Base58);
            let binary_encoding = tx_encoding.into_binary_encoding().ok_or_else(|| {
                Error::invalid_params(format!(
                    "unsupported encoding: {tx_encoding}. Supported encodings: base58, base64"
                ))
            })?;

            let mut decoded_txs: Vec<VersionedTransaction> = Vec::with_capacity(transactions.len());
            for (idx, tx_data) in transactions.iter().enumerate() {
                let (_, tx) = decode_and_deserialize::<VersionedTransaction>(
                    tx_data.clone(),
                    binary_encoding,
                )
                .map_err(|e| Error {
                    code: e.code,
                    message: format!(
                        "Failed to decode bundle transaction {}: {}",
                        idx + 1,
                        e.message
                    ),
                    data: e.data,
                })?;
                decoded_txs.push(tx);
            }

            // -- Phase A: Sandbox execution -------------------------------------------------
            // Take a brief read lock on the original VM to construct a sandbox whose storages
            // are overlay-wrapped, whose subscription registries are empty (no live WS leak),
            // and whose event channels buffer into receivers we hold here.
            let bundle_sandbox = ctx
                .svm_locker
                .with_svm_reader(|svm_reader| svm_reader.clone_for_bundle_sandbox());

            let BundleSandbox {
                svm: sandbox_svm,
                geyser_rx,
                simnet_rx,
            } = bundle_sandbox;

            let sandbox_locker = SurfnetSvmLocker::new(sandbox_svm);

            let remote_ctx = &None;
            let skip_preflight = true;
            let sigverify = true;

            let mut bundle_signatures: Vec<Signature> = Vec::with_capacity(decoded_txs.len());
            for (idx, tx) in decoded_txs.iter().enumerate() {
                let (status_tx, status_rx) = crossbeam_channel::bounded(1);

                // Awaiting directly here lets the surrounding JSON-RPC runtime drive the
                // future. We must NOT use `hiro_system_kit::nestable_block_on` because the
                // HTTP worker thread is already inside a tokio runtime and `block_on` on the
                // current handle panics with "Cannot start a runtime from within a runtime".
                let process_res = sandbox_locker
                    .process_transaction(
                        remote_ctx,
                        tx.clone(),
                        status_tx,
                        skip_preflight,
                        sigverify,
                    )
                    .await;

                bundle_signatures.push(tx.signatures[0]);

                if let Err(e) = process_res {
                    // Dropping `sandbox_locker` discards all overlay state and the cloned
                    // LiteSVM, so the original VM is byte-identical to its pre-bundle state.
                    return Err(Error::invalid_params(format!(
                        "Jito bundle couldn't be executed, failed to process transaction {}: {e}",
                        idx + 1
                    )));
                }

                // `process_transaction` only returns after the sandbox has run the tx and
                // dispatched a status event, so `try_recv`/`recv_timeout` will not actually
                // park the worker for any meaningful time; the 2s timeout is a hard ceiling
                // for an unexpectedly missed status.
                match status_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                    Ok(TransactionStatusEvent::Success(_)) => {}
                    Ok(TransactionStatusEvent::SimulationFailure(other)) => {
                        return Err(Error::invalid_params(format!(
                            "Jito bundle couldn't be executed: simulation failed for transaction {}: {:?}",
                            idx + 1,
                            other
                        )));
                    }
                    Ok(TransactionStatusEvent::ExecutionFailure(other)) => {
                        return Err(Error::invalid_params(format!(
                            "Jito bundle couldn't be executed: Execution failed for transaction {}: {:?}",
                            idx + 1,
                            other
                        )));
                    }
                    Ok(TransactionStatusEvent::VerificationFailure(ver_fail_err)) => {
                        return Err(Error::invalid_params(format!(
                            "Jito bundle couldn't be executed: Verification failed for transaction {}: {:?}",
                            idx + 1,
                            ver_fail_err
                        )));
                    }
                    Err(_) => {
                        return Err(RpcCustomError::NodeUnhealthy {
                            num_slots_behind: None,
                        }
                        .into());
                    }
                }
            }

            // -- Phase B: Atomic commit -----------------------------------------------------
            // All bundle transactions succeeded on the sandbox. Extract the sandbox SVM (the
            // only remaining Arc reference is the local `sandbox_locker`), reassemble the
            // BundleSandbox and call commit_sandbox under the original VM's writer lock.
            let sandbox_svm = match Arc::try_unwrap(sandbox_locker.0) {
                Ok(rwlock) => rwlock.into_inner(),
                Err(_) => {
                    // Should never happen: sandbox_locker was constructed locally and never
                    // shared.
                    return Err(Error::internal_error());
                }
            };
            let reassembled = BundleSandbox {
                svm: sandbox_svm,
                geyser_rx,
                simnet_rx,
            };

            // Use a discardable status channel for the bundle. The runloop will use it to
            // attempt sending Confirmed/Finalized updates; nobody reads it so try_send fails
            // silently.
            let (bundle_status_tx, _bundle_status_rx) = crossbeam_channel::unbounded();

            ctx.svm_locker
                .with_svm_writer(move |original| {
                    original.commit_sandbox(reassembled, bundle_status_tx)
                })
                .map_err(|e| {
                    Error::invalid_params(format!(
                        "Jito bundle commit failed after successful sandbox execution: {e}"
                    ))
                })?;

            // Calculate bundle ID by hashing comma-separated signatures (Jito-compatible)
            // https://github.com/jito-foundation/jito-solana/blob/master/sdk/src/bundle/mod.rs#L21
            let concatenated_signatures = bundle_signatures
                .iter()
                .map(|sig| sig.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut hasher = Sha256::new();
            hasher.update(concatenated_signatures.as_bytes());
            let bundle_id = hex::encode(hasher.finalize());

            ctx.svm_locker.store_bundle(
                bundle_id.clone(),
                bundle_signatures
                    .iter()
                    .map(|sig| sig.to_string())
                    .collect(),
            )?;
            Ok(bundle_id)
        })
    }

    fn get_bundle_statuses(
        &self,
        meta: Self::Metadata,
        bundle_ids: Vec<String>,
    ) -> BoxFuture<Result<RpcResponse<Vec<Option<JitoBundleStatus>>>>> {
        Box::pin(async move {
            if bundle_ids.is_empty() {
                return Err(Error::invalid_params("bundle_ids cannot be empty"));
            }
            if bundle_ids.len() > MAX_BUNDLES_PER_QUERY {
                return Err(Error::invalid_params(format!(
                    "bundle_ids exceeds maximum of {MAX_BUNDLES_PER_QUERY} per request"
                )));
            }

            let Some(ctx) = &meta else {
                return Err(RpcCustomError::NodeUnhealthy {
                    num_slots_behind: None,
                }
                .into());
            };

            // We need a single `context.slot` for the outer RpcResponse. The most accurate slot
            // is the one returned by the underlying `get_signature_statuses` call; if all
            // requested bundles are unknown locally we fall back to the locker's latest slot so
            // the response shape still matches Solana's contextualized RPC contract.
            let mut last_context: Option<RpcResponseContext> = None;
            let mut value: Vec<Option<JitoBundleStatus>> = Vec::with_capacity(bundle_ids.len());

            for bundle_id in bundle_ids {
                let Some(signatures) = ctx.svm_locker.get_bundle(&bundle_id) else {
                    value.push(None);
                    continue;
                };
                if signatures.is_empty() {
                    value.push(None);
                    continue;
                }

                let statuses = super::full::Full::get_signature_statuses(
                    &SurfpoolFullRpc,
                    meta.clone(),
                    signatures.clone(),
                    None,
                )
                .await?;

                last_context = Some(statuses.context.clone());

                // Bundle txs are processed sequentially in one go; they share the same landing
                // slot and confirmation level, so we take slot/status from the first status
                // entry only and aggregate `err` across all entries.
                let (slot, confirmation_status, first_err) = {
                    let mut iter = statuses.value.iter().flatten();

                    let (slot, confirmation_status, head_err) = match iter.next() {
                        Some(first) => (
                            first.slot,
                            first.confirmation_status.clone(),
                            first.err.clone(),
                        ),
                        None => (0, None, None),
                    };

                    let first_err = head_err.or_else(|| iter.find_map(|s| s.err.clone()));
                    (slot, confirmation_status, first_err)
                };

                let confirmation_status =
                    confirmation_status.unwrap_or(TransactionConfirmationStatus::Processed);

                value.push(Some(JitoBundleStatus {
                    bundle_id,
                    transactions: signatures,
                    slot,
                    confirmation_status,
                    err: match first_err {
                        Some(e) => Err(e),
                        None => Ok(()),
                    },
                }));
            }

            let context = last_context.unwrap_or_else(|| {
                let slot = ctx
                    .svm_locker
                    .with_svm_reader(|svm| svm.get_latest_absolute_slot());
                RpcResponseContext::new(slot)
            });

            Ok(RpcResponse { context, value })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use sha2::{Digest, Sha256};
    use solana_keypair::Keypair;
    use solana_message::{VersionedMessage, v0::Message as V0Message};
    use solana_pubkey::Pubkey;
    use solana_signer::Signer;
    use solana_system_interface::instruction as system_instruction;
    use solana_transaction::versioned::VersionedTransaction;
    use solana_transaction_status::TransactionConfirmationStatus as SolanaTxConfirmationStatus;
    use surfpool_types::{SimnetCommand, TransactionConfirmationStatus, TransactionStatusEvent};

    use super::*;
    use crate::{
        tests::helpers::TestSetup,
        types::{SurfnetTransactionStatus, TransactionWithStatusMeta},
    };

    const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

    fn build_v0_transaction(
        payer: &Pubkey,
        signers: &[&Keypair],
        instructions: &[solana_instruction::Instruction],
        recent_blockhash: &solana_hash::Hash,
    ) -> VersionedTransaction {
        let msg = VersionedMessage::V0(
            V0Message::try_compile(payer, instructions, &[], *recent_blockhash).unwrap(),
        );
        VersionedTransaction::try_new(msg, signers).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_empty_bundle_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![], None)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Bundle cannot be empty"),
            "Expected 'Bundle cannot be empty' error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_exceeds_max_size_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let transactions = vec!["tx".to_string(); MAX_BUNDLE_SIZE + 1];
        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), transactions, None)
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("exceeds maximum size"),
            "Expected max size error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_no_context_returns_unhealthy() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .send_bundle(None, vec!["some_tx".to_string()], None)
            .await;

        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_unknown_bundle_returns_null_entry() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let missing_id = "a".repeat(64);
        let response = setup
            .rpc
            .get_bundle_statuses(Some(setup.context), vec![missing_id])
            .await
            .expect("getBundleStatuses should not return a JSON-RPC error");
        assert_eq!(
            response.value.len(),
            1,
            "value array must have one entry per requested bundle id"
        );
        assert!(
            response.value[0].is_none(),
            "unknown bundle_id should appear as a null entry inside `value`, not as an outer null"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_empty_input_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .get_bundle_statuses(Some(setup.context), vec![])
            .await;
        assert!(result.is_err(), "empty bundle_ids should be rejected");
        let err = result.unwrap_err();
        assert!(
            err.message.contains("cannot be empty"),
            "Expected empty-input error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_exceeds_max_per_query_rejected() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let too_many = vec!["a".repeat(64); MAX_BUNDLES_PER_QUERY + 1];
        let result = setup
            .rpc
            .get_bundle_statuses(Some(setup.context), too_many)
            .await;
        assert!(
            result.is_err(),
            "exceeding MAX_BUNDLES_PER_QUERY should error"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("exceeds maximum"),
            "Expected max-batch error, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_no_context_returns_unhealthy() {
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let result = setup
            .rpc
            .get_bundle_statuses(None, vec!["a".repeat(64)])
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_single_transaction() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();
        let expected_sig = tx.signatures[0];

        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![tx_encoded], None)
            .await;

        assert!(result.is_ok(), "Bundle should succeed: {:?}", result);

        // Verify bundle ID is SHA-256 of the signature
        let bundle_id = result.unwrap();
        let mut hasher = Sha256::new();
        hasher.update(expected_sig.to_string().as_bytes());
        let expected_bundle_id = hex::encode(hasher.finalize());
        assert_eq!(
            bundle_id, expected_bundle_id,
            "Bundle ID should match SHA-256 of signature"
        );

        // Verify recipient balance reflects the committed bundle
        let recipient_lamports = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        assert_eq!(
            recipient_lamports, LAMPORTS_PER_SOL,
            "Bundle commit should have applied lamport transfer to recipient"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_multiple_transactions() {
        let payer = Keypair::new();
        let recipient1 = Pubkey::new_unique();
        let recipient2 = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 5 * LAMPORTS_PER_SOL);

        let tx1 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient1,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx2 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient2,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        let tx1_encoded = bs58::encode(bincode::serialize(&tx1).unwrap()).into_string();
        let tx2_encoded = bs58::encode(bincode::serialize(&tx2).unwrap()).into_string();
        let expected_sig1 = tx1.signatures[0];
        let expected_sig2 = tx2.signatures[0];

        let result = setup
            .rpc
            .send_bundle(
                Some(setup.context.clone()),
                vec![tx1_encoded, tx2_encoded],
                None,
            )
            .await;

        assert!(result.is_ok(), "Bundle should succeed: {:?}", result);

        // Both recipient balances should reflect committed bundle
        let recipient1_lamports = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient1))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        let recipient2_lamports = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recipient2))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0);
        assert_eq!(recipient1_lamports, LAMPORTS_PER_SOL);
        assert_eq!(recipient2_lamports, LAMPORTS_PER_SOL);

        // Verify bundle ID is SHA-256 of comma-separated signatures
        let bundle_id = result.unwrap();
        let concatenated = format!("{},{}", expected_sig1, expected_sig2);
        let mut hasher = Sha256::new();
        hasher.update(concatenated.as_bytes());
        let expected_bundle_id = hex::encode(hasher.finalize());
        assert_eq!(
            bundle_id, expected_bundle_id,
            "Bundle ID should match SHA-256 of comma-separated signatures"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_dependent_transaction_failure_aborts_entire_bundle() {
        let payer = Keypair::new();
        let recipient = Keypair::new();

        // Use mempool-backed setup so we can assert that a sandbox failure does NOT enqueue any
        // ProcessTransaction commands
        let (mempool_tx, mempool_rx) = crossbeam_channel::unbounded();
        let setup = TestSetup::new_with_mempool(SurfpoolJitoRpc, mempool_tx);

        // Drain any ProcessTransaction commands so `sendTransaction` cannot block this test even
        // if Phase 2 is accidentally reached. We track whether anything was sent.
        let observed_process_tx = Arc::new(AtomicUsize::new(0));
        let stop_drain = Arc::new(AtomicBool::new(false));
        let observed_process_tx_clone = observed_process_tx.clone();
        let stop_drain_clone = stop_drain.clone();
        let svm_locker_clone = setup.context.svm_locker.clone();
        let drain_handle = hiro_system_kit::thread_named("mempool_drain_dependent_bundle")
            .spawn(move || {
                while !stop_drain_clone.load(Ordering::SeqCst) {
                    let Ok(cmd) = mempool_rx.recv_timeout(Duration::from_millis(200)) else {
                        continue;
                    };
                    match cmd {
                        SimnetCommand::ProcessTransaction(_, tx, status_tx, _, _) => {
                            observed_process_tx_clone.fetch_add(1, Ordering::SeqCst);

                            // Minimal bookkeeping (mirrors other bundle tests) + unblock the RPC.
                            let sig = tx.signatures[0];
                            let mut writer = svm_locker_clone.0.blocking_write();
                            let slot = writer.get_latest_absolute_slot();
                            writer.transactions_queued_for_confirmation.push_back((
                                tx.clone(),
                                status_tx.clone(),
                                None,
                            ));
                            let tx_with_status_meta = TransactionWithStatusMeta {
                                slot,
                                transaction: tx,
                                ..Default::default()
                            };
                            let mutated_accounts = std::collections::HashSet::new();
                            let _ = writer.transactions.store(
                                sig.to_string(),
                                SurfnetTransactionStatus::processed(
                                    tx_with_status_meta,
                                    mutated_accounts,
                                ),
                            );

                            let _ = status_tx.send(TransactionStatusEvent::Success(
                                TransactionConfirmationStatus::Confirmed,
                            ));
                        }
                        _ => continue,
                    }
                }
            })
            .unwrap();

        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer so tx1 can fund the recipient.
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 5 * LAMPORTS_PER_SOL);
        // tx1: payer -> recipient (funds recipient so it can pay fees for tx2)
        let tx1 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient.pubkey(),
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        // tx2 depends on tx1 having executed (recipient needs funds), but must still fail.
        let tx2 = build_v0_transaction(
            &recipient.pubkey(),
            &[&recipient],
            &[system_instruction::transfer(
                &recipient.pubkey(),
                &payer.pubkey(),
                2 * LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        let tx1_encoded = bs58::encode(bincode::serialize(&tx1).unwrap()).into_string();
        let tx2_encoded = bs58::encode(bincode::serialize(&tx2).unwrap()).into_string();

        let result = setup
            .rpc
            .send_bundle(
                Some(setup.context.clone()),
                vec![tx1_encoded, tx2_encoded],
                None,
            )
            .await;

        assert!(
            result.is_err(),
            "Bundle should fail if any sandbox transaction fails"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Jito bundle couldn't be executed"),
            "Expected sandbox failure for tx2, got: {}",
            err.message
        );

        stop_drain.store(true, Ordering::SeqCst);
        let _ = drain_handle.join();

        let recp_pubkey = recipient.pubkey();
        let recp_bal = setup
            .context
            .svm_locker
            .with_svm_reader(|svm| svm.get_account(&recp_pubkey))
            .ok()
            .flatten()
            .map(|a| a.lamports)
            .unwrap_or(0); // this should be fine, since the recp. kp was new, it's not in the svm state

        assert_eq!(
            recp_bal, 0,
            "expected jito bundle to not take effect after bundle failure"
        );

        // If sandbox failure happens as expected, Phase 2 should never run.
        assert_eq!(
            observed_process_tx.load(Ordering::SeqCst),
            0,
            "Expected zero mempool ProcessTransaction commands; sandbox failure should prevent Phase 2"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_simulation_failure_returns_not_atomic_error() {
        let setup = TestSetup::new(SurfpoolJitoRpc);

        // Build a tx that should fail during `simulateTransaction` because the payer
        // has no lamports (no explicit airdrop in this test).
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();

        let result = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![tx_encoded], None)
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();

        assert!(
            err.message.contains("Jito bundle couldn't be executed"),
            "Expected not-atomic error, got: {}",
            err.message
        );
        assert!(
            err.message.contains("Jito bundle couldn't be executed:"),
            "Expected simulation-failure error for transaction 1, got: {}",
            err.message
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_send_bundle_persists_bundle_signatures() {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let (mempool_tx, _) = crossbeam_channel::unbounded();
        let setup = TestSetup::new_with_mempool(SurfpoolJitoRpc, mempool_tx);

        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        // Airdrop to payer so tx can succeed in our manual processing
        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();

        // Build expected signatures locally (what we expect to be persisted under bundle_id)
        let expected_sigs = vec![tx.signatures[0].to_string()];

        let setup_clone = setup.clone();
        let send_bundle_result = setup_clone
            .rpc
            .send_bundle(Some(setup_clone.context), vec![tx_encoded], None)
            .await;

        assert!(send_bundle_result.is_ok(), "Expected send_bundle to pass");

        let bundle_id = send_bundle_result.unwrap();

        // sendBundle stores bundle signatures directly in `jito_bundles`; poll until visible.
        let started = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(2);
        let persisted = loop {
            match setup.context.svm_locker.get_bundle(&bundle_id) {
                Some(sigs) if !sigs.is_empty() => break sigs,
                _ if started.elapsed() > timeout => {
                    panic!("timed out waiting for bundle to be persisted: {bundle_id}");
                }
                _ => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        assert!(
            !persisted.is_empty(),
            "svm_locker.get_bundle(bundle_id) should not be empty"
        );
        assert_eq!(
            persisted, expected_sigs,
            "Persisted bundle signatures should match locally built signatures"
        );

        let started = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(2);
        let (bundle, context_slot) = loop {
            let response = setup
                .rpc
                .get_bundle_statuses(Some(setup.context.clone()), vec![bundle_id.clone()])
                .await
                .expect("getBundleStatuses should succeed");

            assert_eq!(
                response.value.len(),
                1,
                "getBundleStatuses should return a single status entry per requested id"
            );

            let context_slot = response.context.slot;
            let bundle = response
                .value
                .into_iter()
                .next()
                .unwrap()
                .expect("bundle should exist locally after sendBundle");
            if bundle.slot != 0 {
                break (bundle, context_slot);
            }

            if started.elapsed() > timeout {
                break (bundle, context_slot);
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        assert!(
            context_slot >= bundle.slot,
            "response.context.slot ({}) should be >= bundle.slot ({}); \
             getBundleStatuses must surface the same context slot as the \
             underlying getSignatureStatuses call",
            context_slot,
            bundle.slot,
        );

        assert_eq!(bundle.bundle_id, bundle_id, "bundle_id should match");
        assert_eq!(
            bundle.transactions, expected_sigs,
            "transactions should match bundle signatures"
        );
        assert!(
            matches!(
                bundle.confirmation_status,
                SolanaTxConfirmationStatus::Processed
                    | SolanaTxConfirmationStatus::Confirmed
                    | SolanaTxConfirmationStatus::Finalized
            ),
            "confirmation_status should be a valid Solana status"
        );
        assert!(bundle.err.is_ok(), "err should be Ok for successful bundle");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_multi_transaction_bundle() {
        let payer = Keypair::new();
        let recipient1 = Pubkey::new_unique();
        let recipient2 = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 5 * LAMPORTS_PER_SOL);

        let tx1 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient1,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx2 = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient2,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );

        let tx1_encoded = bs58::encode(bincode::serialize(&tx1).unwrap()).into_string();
        let tx2_encoded = bs58::encode(bincode::serialize(&tx2).unwrap()).into_string();
        let expected_sigs = vec![tx1.signatures[0].to_string(), tx2.signatures[0].to_string()];

        let bundle_id = setup
            .rpc
            .send_bundle(
                Some(setup.context.clone()),
                vec![tx1_encoded, tx2_encoded],
                None,
            )
            .await
            .expect("sendBundle should succeed for a valid 2-tx bundle");

        let response = setup
            .rpc
            .get_bundle_statuses(Some(setup.context.clone()), vec![bundle_id.clone()])
            .await
            .expect("getBundleStatuses should succeed");

        // Multi-tx bundle must still aggregate into exactly one JitoBundleStatus, with the
        // signatures preserved in submission order.
        assert_eq!(
            response.value.len(),
            1,
            "value array must have one entry per requested bundle id"
        );
        let bundle = response
            .value
            .into_iter()
            .next()
            .unwrap()
            .expect("bundle should exist locally after sendBundle");
        assert_eq!(bundle.bundle_id, bundle_id);
        assert_eq!(
            bundle.transactions, expected_sigs,
            "transactions must preserve submission order across all txs in the bundle"
        );
        assert!(
            bundle.err.is_ok(),
            "successful multi-tx bundle should report Ok"
        );
        assert!(
            matches!(
                bundle.confirmation_status,
                SolanaTxConfirmationStatus::Processed
                    | SolanaTxConfirmationStatus::Confirmed
                    | SolanaTxConfirmationStatus::Finalized
            ),
            "confirmation_status should be a valid Solana status"
        );
    }

    #[test]
    fn test_jito_bundle_status_json_shape() {
        use solana_transaction_error::TransactionError;

        // -- Ok case: field names must be snake_case (Jito wire-compatible) and err must
        // serialize as {"Ok": null}. --
        let ok_status = JitoBundleStatus {
            bundle_id: "abc123".to_string(),
            transactions: vec!["sig1".to_string(), "sig2".to_string()],
            slot: 42,
            confirmation_status: SolanaTxConfirmationStatus::Finalized,
            err: Ok(()),
        };
        let json = serde_json::to_value(&ok_status).expect("JitoBundleStatus should serialize");

        assert!(
            json.get("bundle_id").is_some(),
            "expected snake_case `bundle_id` field, got: {json}"
        );
        assert!(json.get("transactions").is_some());
        assert!(json.get("slot").is_some());
        assert!(
            json.get("confirmationStatus").is_some(),
            "expected snake_case `confirmationStatus` field, got: {json}"
        );
        assert!(json.get("err").is_some());

        assert!(
            json.get("bundleId").is_none(),
            "camelCase `bundleId` should not be serialized (Jito uses snake_case on the wire)"
        );
        assert!(
            json.get("confirmation_status").is_none(),
            "camelCase `confirmation_status` should not be serialized"
        );

        // err must serialize as {"Ok": null} for a successful bundle.
        assert_eq!(
            json.get("err"),
            Some(&serde_json::json!({ "Ok": null })),
            "Ok variant of err should serialize as {{\"Ok\": null}}"
        );
        assert_eq!(json.get("bundle_id").unwrap().as_str(), Some("abc123"));
        assert_eq!(json.get("slot").unwrap().as_u64(), Some(42));
        assert_eq!(
            json.get("confirmationStatus").unwrap().as_str(),
            Some("finalized"),
            "confirmationStatus should serialize as a lowercase string"
        );

        // -- Err case: err must serialize as {"Err": ...} carrying the inner TransactionError. --
        let err_status = JitoBundleStatus {
            bundle_id: "abc123".to_string(),
            transactions: vec!["sig1".to_string()],
            slot: 7,
            confirmation_status: SolanaTxConfirmationStatus::Processed,
            err: Err(TransactionError::AccountNotFound),
        };
        let err_json = serde_json::to_value(&err_status).expect("err variant should serialize");
        let err_field = err_json.get("err").expect("err field should be present");
        assert!(
            err_field.get("Err").is_some(),
            "Err variant of err should serialize as {{\"Err\": ...}}, got: {err_field}"
        );

        // Round-trip: deserializing must yield the same struct.
        let round_tripped: JitoBundleStatus =
            serde_json::from_value(json).expect("JitoBundleStatus should round-trip");
        assert_eq!(round_tripped, ok_status);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_get_bundle_statuses_batched_known_and_unknown() {
        // Submit one real bundle, then call getBundleStatuses with a batch containing the real
        // id plus an unknown id. The response must preserve order and include `null` at the
        // unknown index, matching Jito's wire contract.
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let setup = TestSetup::new(SurfpoolJitoRpc);
        let recent_blockhash = setup
            .context
            .svm_locker
            .with_svm_reader(|svm_reader| svm_reader.latest_blockhash());

        let _ = setup
            .context
            .svm_locker
            .0
            .write()
            .await
            .airdrop(&payer.pubkey(), 2 * LAMPORTS_PER_SOL);

        let tx = build_v0_transaction(
            &payer.pubkey(),
            &[&payer],
            &[system_instruction::transfer(
                &payer.pubkey(),
                &recipient,
                LAMPORTS_PER_SOL,
            )],
            &recent_blockhash,
        );
        let tx_encoded = bs58::encode(bincode::serialize(&tx).unwrap()).into_string();

        let known_id = setup
            .rpc
            .send_bundle(Some(setup.context.clone()), vec![tx_encoded], None)
            .await
            .expect("sendBundle should succeed");
        let unknown_id = "f".repeat(64);

        // Order: [unknown, known] so we also verify positional null-vs-Some mapping isn't
        // accidentally first-only.
        let response = setup
            .rpc
            .get_bundle_statuses(
                Some(setup.context.clone()),
                vec![unknown_id.clone(), known_id.clone()],
            )
            .await
            .expect("getBundleStatuses should succeed");

        assert_eq!(
            response.value.len(),
            2,
            "value must have exactly one entry per requested bundle id"
        );
        assert!(
            response.value[0].is_none(),
            "index 0 (unknown id) should be null"
        );
        let known = response.value[1]
            .as_ref()
            .expect("index 1 (known id) should be Some(JitoBundleStatus)");
        assert_eq!(known.bundle_id, known_id);
    }
}
