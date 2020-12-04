use crate::consensus::traits::Consensus;
use crate::tx_pool::TxPool;
use anyhow::{anyhow, Result};
use ckb_types::{
    bytes::Bytes,
    packed::{Script, Transaction, WitnessArgs, WitnessArgsReader},
    prelude::Unpack,
};
use gw_common::{
    h256_ext::H256Ext, merkle_utils::calculate_merkle_root, smt::Blake2bHasher, sparse_merkle_tree,
    state::State, H256,
};
use gw_config::ChainConfig;
use gw_generator::{
    generator::{DepositionRequest, StateTransitionArgs, WithdrawalRequest},
    Error as GeneratorError, Generator,
};
use gw_store::{Store, WrapStore};
use gw_types::{
    packed::{
        AccountMerkleState, BlockMerkleState, CancelChallenge, GlobalState, L2Block, L2BlockReader,
        RawL2Block, StartChallenge, SubmitTransactions,
    },
    prelude::{
        Builder as GWBuilder, Entity as GWEntity, Pack as GWPack, PackVec as GWPackVec,
        Reader as GWReader, Unpack as GWUnpack,
    },
};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::SystemTime;

/// Rollup status
#[derive(Debug, Eq, PartialEq)]
pub enum Status {
    Running,
    Halting,
}

/// Produce block param
pub struct ProduceBlockParam {
    /// aggregator of this block
    pub aggregator_id: u32,
    /// deposition requests
    pub deposition_requests: Vec<DepositionRequest>,
    /// user step2 withdrawal requests, collected from RPC
    pub withdrawal_requests: Vec<WithdrawalRequest>,
}

/// sync params
pub struct SyncParam {
    // contains transitions from tip to fork point
    pub reverts: Vec<SyncTransition>,
    /// contains transitions from fork point to new tips
    pub updates: Vec<SyncTransition>,
}

pub struct SyncTransition {
    /// transaction info
    pub transaction_info: TransactionInfo,
    /// transactions' header info
    pub header_info: HeaderInfo,
    /// deposition requests
    pub deposition_requests: Vec<DepositionRequest>,
    /// withdrawal requests
    pub withdrawal_requests: Vec<WithdrawalRequest>,
}

pub struct TransactionInfo {
    pub transaction: Transaction,
    pub block_hash: [u8; 32],
}
pub struct HeaderInfo {
    pub number: u64,
    pub block_hash: [u8; 32],
}

pub struct L2BlockWithState {
    pub block: L2Block,
    pub global_state: GlobalState,
}

/// sync method returned events
pub enum SyncEvent {
    // success
    Success,
    // found a invalid block
    BadBlock(StartChallenge),
    // found a invalid challenge
    BadChallenge(CancelChallenge),
    // the rollup is in a challenge
    WaitChallenge,
}

/// concrete type aliases
pub type StateStore = sparse_merkle_tree::default_store::DefaultStore<sparse_merkle_tree::H256>;
pub type TxPoolImpl = TxPool<WrapStore<StateStore>>;

pub struct Chain<Consensus> {
    rollup_type_script_hash: [u8; 32],
    store: Store<StateStore>,
    last_synced: HeaderInfo,
    tip: L2Block,
    generator: Generator,
    tx_pool: Arc<Mutex<TxPoolImpl>>,
    consensus: Consensus,
}

impl<C: Consensus> Chain<C> {
    pub fn new(
        config: ChainConfig,
        store: Store<StateStore>,
        consensus: C,
        tip: L2Block,
        last_synced: HeaderInfo,
        generator: Generator,
        tx_pool: Arc<Mutex<TxPoolImpl>>,
    ) -> Self {
        let rollup_type_script: Script = config.rollup_type_script.clone().into();
        let rollup_type_script_hash = rollup_type_script.calc_script_hash().unpack();
        Chain {
            store,
            last_synced,
            tip,
            generator,
            tx_pool,
            consensus,
            rollup_type_script_hash,
        }
    }

    pub fn tip(&self) -> &L2Block {
        &self.tip
    }

    pub fn store(&self) -> &Store<StateStore> {
        &self.store
    }

    pub fn last_synced(&self) -> &HeaderInfo {
        &self.last_synced
    }

    /// return rollup status
    pub fn status(&self) -> Status {
        unimplemented!()
    }

    /// Sync chain from layer1
    pub fn sync(&mut self, param: SyncParam) -> Result<SyncEvent> {
        // TODO handle layer1 reorg
        if !param.reverts.is_empty() {
            panic!("layer1 chain has forked!")
        }
        // check status
        if self.status() == Status::Halting {
            // TODO validate challenge request, return BadChallenge if challenge is invalid, otherwise return WaitChallenge
        }
        // apply tx to state
        for sync in param.updates {
            let SyncTransition {
                transaction_info,
                header_info,
                deposition_requests,
                withdrawal_requests,
            } = sync;
            debug_assert_eq!(transaction_info.block_hash, header_info.block_hash);
            let block_number: u64 = header_info.number;
            assert!(
                block_number > self.last_synced.number,
                "must greater than last synced number"
            );

            // parse layer2 block
            let l2block =
                parse_l2block(&transaction_info.transaction, &self.rollup_type_script_hash)?;

            let tip_number: u64 = self.tip.raw().number().unpack();
            assert!(
                l2block.raw().number().unpack() == tip_number + 1,
                "new l2block number must be the successor of the tip"
            );

            // process l2block
            let args = StateTransitionArgs {
                l2block: l2block.clone(),
                deposition_requests,
                withdrawal_requests,
            };
            // process transactions
            if let Err(err) = self.generator.apply_state_transition(&mut self.store, args) {
                // handle tx error
                match err {
                    GeneratorError::Transaction(err) => {
                        // TODO run offchain validator before send challenge, to make sure the block is bad
                        return Ok(SyncEvent::BadBlock(err.challenge_context));
                    }
                    err => return Err(err.into()),
                }
            }
            self.store.insert_block(l2block.clone())?;
            self.store.attach_block(l2block.clone())?;

            // update chain
            self.last_synced = header_info;
            self.tip = l2block;
        }
        // update tx pool state
        let overlay_state = self.store.new_overlay()?;
        let nb_ctx = self.consensus.next_block_context(&self.tip);
        self.tx_pool
            .lock()
            .update_tip(&self.tip, overlay_state, nb_ctx)?;
        Ok(SyncEvent::Success)
    }

    /// Produce an unsigned new block
    ///
    /// This function should be called in the turn that the current aggregator to produce the next block,
    /// otherwise the produced block may invalided by the state-validator contract.
    pub fn produce_block(&mut self, param: ProduceBlockParam) -> Result<L2BlockWithState> {
        let ProduceBlockParam {
            aggregator_id,
            deposition_requests,
            withdrawal_requests,
        } = param;
        // take txs from tx pool
        // produce block
        let pkg = self
            .tx_pool
            .lock()
            .package_txs(&deposition_requests, &withdrawal_requests)?;
        let parent_number: u64 = self.tip.raw().number().unpack();
        let number = parent_number + 1;
        let timestamp: u64 = unixtime()?;
        let submit_txs = {
            let tx_witness_root = calculate_merkle_root(
                pkg.tx_recipts
                    .iter()
                    .map(|tx_recipt| &tx_recipt.tx_witness_hash)
                    .cloned()
                    .collect(),
            )
            .map_err(|err| anyhow!("merkle root error: {:?}", err))?;
            let tx_count = pkg.tx_recipts.len() as u32;
            let compacted_post_root_list: Vec<_> = pkg
                .tx_recipts
                .iter()
                .map(|tx_recipt| &tx_recipt.compacted_post_account_root)
                .cloned()
                .collect();
            SubmitTransactions::new_builder()
                .tx_witness_root(tx_witness_root.pack())
                .tx_count(tx_count.pack())
                .compacted_post_root_list(compacted_post_root_list.pack())
                .build()
        };
        let prev_root: [u8; 32] = pkg.prev_account_state.root.into();
        let prev_account = AccountMerkleState::new_builder()
            .merkle_root(prev_root.pack())
            .count(pkg.prev_account_state.count.pack())
            .build();
        let post_root: [u8; 32] = pkg.post_account_state.root.into();
        let post_account = AccountMerkleState::new_builder()
            .merkle_root(post_root.pack())
            .count(pkg.post_account_state.count.pack())
            .build();
        let raw_block = RawL2Block::new_builder()
            .number(number.pack())
            .aggregator_id(aggregator_id.pack())
            .timestamp(timestamp.pack())
            .post_account(post_account.clone())
            .prev_account(prev_account)
            .submit_transactions(Some(submit_txs).pack())
            .build();
        // generate block fields from current state
        let kv_state: Vec<(H256, H256)> = pkg
            .touched_keys
            .iter()
            .map(|k| {
                self.store
                    .get_raw(k)
                    .map(|v| (*k, v))
                    .map_err(|err| anyhow!("can't fetch value error: {:?}", err))
            })
            .collect::<Result<_>>()?;
        let packed_kv_state = kv_state
            .iter()
            .map(|(k, v)| {
                let k: [u8; 32] = (*k).into();
                let v: [u8; 32] = (*v).into();
                (k, v)
            })
            .collect::<Vec<_>>()
            .pack();
        let proof = self
            .store
            .account_smt()
            .merkle_proof(kv_state.iter().map(|(k, _v)| *k).collect())
            .map_err(|err| anyhow!("merkle proof error: {:?}", err))?
            .compile(kv_state)?
            .0;
        let txs: Vec<_> = pkg.tx_recipts.into_iter().map(|tx| tx.tx).collect();
        let block_proof = self
            .store
            .block_smt()
            .merkle_proof(vec![H256::from_u64(number)])
            .map_err(|err| anyhow!("merkle proof error: {:?}", err))?
            .compile(vec![(H256::from_u64(number), H256::zero())])?;
        let block = L2Block::new_builder()
            .raw(raw_block)
            .kv_state(packed_kv_state)
            .kv_state_proof(proof.pack())
            .transactions(txs.pack())
            .block_proof(block_proof.0.pack())
            .build();
        let post_block = {
            let post_block_root: [u8; 32] = block_proof
                .compute_root::<Blake2bHasher>(vec![(block.smt_key().into(), block.hash().into())])?
                .into();
            let block_count = number + 1;
            BlockMerkleState::new_builder()
                .merkle_root(post_block_root.pack())
                .count(block_count.pack())
                .build()
        };
        let global_state = GlobalState::new_builder()
            .account(post_account)
            .block(post_block)
            .build();
        Ok(L2BlockWithState {
            block,
            global_state,
        })
    }
}

fn unixtime() -> Result<u64> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(Into::into)
}

fn parse_l2block(tx: &Transaction, rollup_id: &[u8; 32]) -> Result<L2Block> {
    // find rollup state cell from outputs
    let (i, _) = tx
        .raw()
        .outputs()
        .into_iter()
        .enumerate()
        .find(|(_i, output)| {
            output
                .type_()
                .to_opt()
                .map(|type_| type_.calc_script_hash().unpack())
                .as_ref()
                == Some(rollup_id)
        })
        .ok_or_else(|| anyhow!("no rollup cell found"))?;

    let witness: Bytes = tx
        .witnesses()
        .get(i)
        .ok_or_else(|| anyhow!("no witness"))?
        .unpack();
    let witness_args = match WitnessArgsReader::verify(&witness, false) {
        Ok(_) => WitnessArgs::new_unchecked(witness),
        Err(_) => {
            return Err(anyhow!("invalid witness"));
        }
    };
    let output_type: Bytes = witness_args
        .output_type()
        .to_opt()
        .ok_or_else(|| anyhow!("output_type field is none"))?
        .unpack();
    match L2BlockReader::verify(&output_type, false) {
        Ok(_) => Ok(L2Block::new_unchecked(output_type)),
        Err(_) => Err(anyhow!("invalid l2block")),
    }
}