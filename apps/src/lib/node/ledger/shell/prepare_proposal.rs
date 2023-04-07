//! Implementation of the [`RequestPrepareProposal`] ABCI++ method for the Shell

use namada::ledger::storage::{DBIter, StorageHasher, DB};
use namada::proto::Tx;
use namada::types::internal::TxInQueue;
use namada::types::transaction::tx_types::TxType;
use namada::types::transaction::wrapper::wrapper_tx::PairingEngine;
use namada::types::transaction::{AffineCurve, DecryptedTx, EllipticCurve};
use namada::types::hash::Hash;
use sha2::{Digest, Sha256};

use super::super::*;
use crate::facade::tendermint_proto::abci::RequestPrepareProposal;
#[cfg(feature = "abcipp")]
use crate::facade::tendermint_proto::abci::{tx_record::TxAction, TxRecord};
use crate::node::ledger::shell::{process_tx, ShellMode};
use crate::node::ledger::shims::abcipp_shim_types::shim::TxBytes;

// TODO: remove this hard-coded value; Tendermint, and thus
// Namada uses 20 MiB max block sizes by default; 5 MiB leaves
// plenty of room for header data, evidence and protobuf serialization
// overhead
const MAX_PROPOSAL_SIZE: usize = 5 << 20;
const HALF_MAX_PROPOSAL_SIZE: usize = MAX_PROPOSAL_SIZE / 2;

impl<D, H> Shell<D, H>
where
    D: DB + for<'iter> DBIter<'iter> + Sync + 'static,
    H: StorageHasher + Sync + 'static,
{
    /// Begin a new block.
    ///
    /// We fill half the block space with new wrapper txs given to us
    /// from the mempool by tendermint. The rest of the block is filled
    /// with decryptions of the wrapper txs from the previously
    /// committed block.
    ///
    /// INVARIANT: Any changes applied in this method must be reverted if
    /// the proposal is rejected (unless we can simply overwrite
    /// them in the next block).
    pub fn prepare_proposal(
        &self,
        req: RequestPrepareProposal,
    ) -> response::PrepareProposal {
        let txs = if let ShellMode::Validator { .. } = self.mode {
            // TODO: This should not be hardcoded
            let privkey = <EllipticCurve as PairingEngine>::G2Affine::prime_subgroup_generator();

            // TODO: Craft the Ethereum state update tx
            // filter in half of the new txs from Tendermint, only keeping
            // wrappers
            let mut total_proposal_size = 0;
            #[cfg(feature = "abcipp")]
            let mut txs: Vec<TxRecord> = req
                .txs
                .into_iter()
                .map(|tx_bytes| {
                    if let Ok(Ok(TxType::Wrapper(_))) =
                        Tx::try_from(tx_bytes.as_slice()).map(process_tx)
                    {
                        record::keep(tx_bytes)
                    } else {
                        record::remove(tx_bytes)
                    }
                })
                .take_while(|tx_record| {
                    let new_size = total_proposal_size + tx_record.tx.len();
                    if new_size > HALF_MAX_PROPOSAL_SIZE
                        || tx_record.action != TxAction::Unmodified as i32
                    {
                        false
                    } else {
                        total_proposal_size = new_size;
                        true
                    }
                })
                .collect();
            #[cfg(not(feature = "abcipp"))]
            let mut txs: Vec<TxBytes> = req
                .txs
                .into_iter()
                .filter_map(|tx_bytes| {
                    if let Ok(Ok(TxType::Wrapper(_))) =
                        Tx::try_from(tx_bytes.as_slice()).map(|x| process_tx(&x).map(Tx::header))
                    {
                        Some(tx_bytes)
                    } else {
                        None
                    }
                })
                .take_while(|tx_bytes| {
                    let new_size = total_proposal_size + tx_bytes.len();
                    if new_size > HALF_MAX_PROPOSAL_SIZE {
                        false
                    } else {
                        total_proposal_size = new_size;
                        true
                    }
                })
                .collect();

            // decrypt the wrapper txs included in the previous block
            let decrypted_txs = self.storage.tx_queue.iter().map(
                |TxInQueue {
                     tx,
                     inner_tx,
                     #[cfg(not(feature = "mainnet"))]
                     has_valid_pow,
                }| {
                    let mut inner_tx = inner_tx.clone();
                    match inner_tx.decrypt(privkey).ok()
                    {
                        Some(()) => {
                            let mut inner_tx = inner_tx.clone();
                            inner_tx.outer_data = TxType::Decrypted(DecryptedTx::Decrypted {
                                header_hash: inner_tx.header_hash(),
                                code_hash: tx.code_hash.clone(),
                                data_hash: tx.data_hash.clone(),
                                #[cfg(not(feature = "mainnet"))]
                                has_valid_pow: *has_valid_pow,
                            });
                            inner_tx
                        },
                        // An absent or undecryptable inner_tx are both
                        // treated as undecryptable
                        None => {
                            let mut inner_tx = inner_tx.clone();
                            inner_tx.outer_data = TxType::Decrypted(
                                DecryptedTx::Undecryptable(tx.clone())
                            );
                            inner_tx
                        },
                    }.to_bytes()
                },
            );
            #[cfg(feature = "abcipp")]
            let mut decrypted_txs: Vec<_> =
                decrypted_txs.map(record::add).collect();
            #[cfg(not(feature = "abcipp"))]
            let mut decrypted_txs: Vec<_> = decrypted_txs.collect();

            txs.append(&mut decrypted_txs);
            txs
        } else {
            vec![]
        };

        #[cfg(feature = "abcipp")]
        {
            response::PrepareProposal {
                tx_records: txs,
                ..Default::default()
            }
        }
        #[cfg(not(feature = "abcipp"))]
        {
            response::PrepareProposal { txs }
        }
    }
}

/// Functions for creating the appropriate TxRecord given the
/// numeric code
#[cfg(feature = "abcipp")]
pub(super) mod record {
    use super::*;

    /// Keep this transaction in the proposal
    pub fn keep(tx: TxBytes) -> TxRecord {
        TxRecord {
            action: TxAction::Unmodified as i32,
            tx,
        }
    }

    /// A transaction added to the proposal not provided by
    /// Tendermint from the mempool
    pub fn add(tx: TxBytes) -> TxRecord {
        TxRecord {
            action: TxAction::Added as i32,
            tx,
        }
    }

    /// Remove this transaction from the set provided
    /// by Tendermint from the mempool
    pub fn remove(tx: TxBytes) -> TxRecord {
        TxRecord {
            action: TxAction::Removed as i32,
            tx,
        }
    }
}

#[cfg(test)]
mod test_prepare_proposal {
    use borsh::BorshSerialize;
    use namada::types::storage::Epoch;
    use namada::types::transaction::{Fee, WrapperTx};
    use namada::proto::InnerTx;
    use namada::proto::{SignedOuterTxData, SignedTxData, Code, Data, Section, Signature};

    use super::*;
    use crate::node::ledger::shell::test_utils::{gen_keypair, TestShell};

    /// Test that if a tx from the mempool is not a
    /// WrapperTx type, it is not included in the
    /// proposed block.
    /*#[test]
    fn test_prepare_proposal_rejects_non_wrapper_tx() {
        let (shell, _) = TestShell::new();
        let tx = Tx::new(
            "wasm_code".as_bytes().to_owned(),
            Some(SignedOuterTxData {data: Some("transaction_data".as_bytes().to_owned()), sig: None}),
        );
        let req = RequestPrepareProposal {
            txs: vec![tx.to_bytes()],
            max_tx_bytes: 0,
            ..Default::default()
        };
        #[cfg(feature = "abcipp")]
        assert_eq!(
            shell.prepare_proposal(req).tx_records,
            vec![record::remove(tx.to_bytes())]
        );
        #[cfg(not(feature = "abcipp"))]
        assert!(shell.prepare_proposal(req).txs.is_empty());
    }*/

    /// Test that if an error is encountered while
    /// trying to process a tx from the mempool,
    /// we simply exclude it from the proposal
    #[test]
    fn test_error_in_processing_tx() {
        let (shell, _) = TestShell::new();
        let keypair = gen_keypair();
        // an unsigned wrapper will cause an error in processing
        let mut wrapper = Tx::new(
            TxType::Wrapper(WrapperTx::new(
                Fee {
                    amount: 0.into(),
                    token: shell.storage.native_token.clone(),
                },
                &keypair,
                Epoch(0),
                0.into(),
                #[cfg(not(feature = "mainnet"))]
                None,
            ))
        );
        wrapper.set_code(Code::new("wasm_code".as_bytes().to_owned()));
        wrapper.set_data(Data::new("transaction_data".as_bytes().to_owned()));
        wrapper.encrypt(&Default::default());
        let wrapper = wrapper.to_bytes();
        #[allow(clippy::redundant_clone)]
        let req = RequestPrepareProposal {
            txs: vec![wrapper.clone()],
            max_tx_bytes: 0,
            ..Default::default()
        };
        #[cfg(feature = "abcipp")]
        assert_eq!(
            shell.prepare_proposal(req).tx_records,
            vec![record::remove(wrapper)]
        );
        #[cfg(not(feature = "abcipp"))]
        assert!(shell.prepare_proposal(req).txs.is_empty());
    }

    /// Test that the decrypted txs are included
    /// in the proposal in the same order as their
    /// corresponding wrappers
    #[test]
    fn test_decrypted_txs_in_correct_order() {
        let (mut shell, _) = TestShell::new();
        let keypair = gen_keypair();
        let mut expected_wrapper = vec![];
        let mut expected_decrypted = vec![];

        let mut req = RequestPrepareProposal {
            txs: vec![],
            max_tx_bytes: 0,
            ..Default::default()
        };
        // create a request with two new wrappers from mempool and
        // two wrappers from the previous block to be decrypted
        for i in 0..2 {
            let mut tx = Tx::new(TxType::Wrapper(WrapperTx::new(
                Fee {
                    amount: 0.into(),
                    token: shell.storage.native_token.clone(),
                },
                &keypair,
                Epoch(0),
                0.into(),
                #[cfg(not(feature = "mainnet"))]
                None,
            )));
            tx.set_code(Code::new("wasm_code".as_bytes().to_owned()));
            tx.set_data(Data::new(format!("transaction data: {}", i).as_bytes().to_owned()));
            tx.add_section(Section::Signature(Signature::new(&tx.header_hash(), &keypair)));
            tx.encrypt(&Default::default());
            shell.enqueue_tx(tx.header().wrapper().expect("expected wrapper"), tx.clone());
            expected_wrapper.push(tx.clone());
            req.txs.push(tx.to_bytes());
            let decrypted_tx = TxType::Decrypted(DecryptedTx::Decrypted {
                header_hash: tx.header_hash(),
                code_hash: tx.code_hash().clone(),
                data_hash: tx.data_hash().clone(),
                #[cfg(not(feature = "mainnet"))]
                has_valid_pow: false,
            });
            std::mem::replace(
                &mut tx.outer_data,
                decrypted_tx,
            );
            expected_decrypted.push(tx.clone());
        }
        // we extract the inner data from the txs for testing
        // equality since otherwise changes in timestamps would
        // fail the test
        expected_wrapper.append(&mut expected_decrypted);
        let expected_txs: Vec<TxType> = expected_wrapper
            .iter()
            .map(|tx| tx.outer_data.clone())
            .collect();
        #[cfg(feature = "abcipp")]
        {
            let received: Vec<Vec<u8>> = shell
                .prepare_proposal(req)
                .tx_records
                .iter()
                .filter_map(
                    |TxRecord {
                         tx: tx_bytes,
                         action,
                     }| {
                        if *action == (TxAction::Unmodified as i32)
                            || *action == (TxAction::Added as i32)
                        {
                            Some(
                                Tx::try_from(tx_bytes.as_slice())
                                    .expect("Test failed")
                                    .outer_data
                                    .expect("Test failed"),
                            )
                        } else {
                            None
                        }
                    },
                )
                .collect();
            // check that the order of the txs is correct
            assert_eq!(received, expected_txs);
        }
        #[cfg(not(feature = "abcipp"))]
        {
            let received: Vec<TxType> = shell
                .prepare_proposal(req)
                .txs
                .into_iter()
                .map(|tx_bytes| {
                    Tx::try_from(tx_bytes.as_slice())
                        .expect("Test failed")
                        .outer_data
                })
                .collect();
            // check that the order of the txs is correct
            assert_eq!(
                received.iter().map(|x| x.try_to_vec().unwrap()).collect::<Vec<_>>(),
                expected_txs.iter().map(|x| x.try_to_vec().unwrap()).collect::<Vec<_>>(),
            );
        }
    }
}
