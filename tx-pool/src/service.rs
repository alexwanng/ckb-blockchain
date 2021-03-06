use crate::block_assembler::BlockAssembler;
use crate::component::entry::TxEntry;
use crate::error::handle_try_send_error;
use crate::pool::{TxPool, TxPoolInfo};
use crate::process::PlugTarget;
use ckb_app_config::{BlockAssemblerConfig, TxPoolConfig};
use ckb_async_runtime::{new_runtime, Handle};
use ckb_error::Error;
use ckb_fee_estimator::FeeRate;
use ckb_jsonrpc_types::BlockTemplate;
use ckb_logger::error;
use ckb_snapshot::{Snapshot, SnapshotMgr};
use ckb_stop_handler::{SignalSender, StopHandler};
use ckb_types::{
    core::{BlockView, Cycle, TransactionView, UncleBlockView, Version},
    packed::ProposalShortId,
};
use ckb_verification::cache::{CacheEntry, TxVerifyCache};
use failure::Error as FailureError;
use faketime::unix_time_as_millis;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{atomic::AtomicU64, Arc};
use tokio::sync::{mpsc, oneshot, RwLock};

pub const DEFAULT_CHANNEL_SIZE: usize = 512;

pub struct Request<A, R> {
    pub responder: crossbeam_channel::Sender<R>,
    pub arguments: A,
}

impl<A, R> Request<A, R> {
    pub fn call(arguments: A, responder: crossbeam_channel::Sender<R>) -> Request<A, R> {
        Request {
            responder,
            arguments,
        }
    }
}

pub struct Notify<A> {
    pub arguments: A,
}

impl<A> Notify<A> {
    pub fn notify(arguments: A) -> Notify<A> {
        Notify { arguments }
    }
}

pub type BlockTemplateResult = Result<BlockTemplate, FailureError>;
type BlockTemplateArgs = (Option<u64>, Option<u64>, Option<Version>);

pub type SubmitTxsResult = Result<Vec<CacheEntry>, Error>;
type NotifyTxsCallback = Option<Box<dyn FnOnce(SubmitTxsResult) + Send + Sync + 'static>>;

type FetchTxRPCResult = Option<(bool, TransactionView)>;

type FetchTxsWithCyclesResult = Vec<(ProposalShortId, (TransactionView, Cycle))>;

pub type ChainReorgArgs = (
    VecDeque<BlockView>,
    VecDeque<BlockView>,
    HashSet<ProposalShortId>,
    Arc<Snapshot>,
);

pub enum Message {
    BlockTemplate(Request<BlockTemplateArgs, BlockTemplateResult>),
    SubmitTxs(Request<Vec<TransactionView>, SubmitTxsResult>),
    NotifyTxs(Notify<(Vec<TransactionView>, NotifyTxsCallback)>),
    ChainReorg(Notify<ChainReorgArgs>),
    FreshProposalsFilter(Request<Vec<ProposalShortId>, Vec<ProposalShortId>>),
    FetchTxs(Request<Vec<ProposalShortId>, HashMap<ProposalShortId, TransactionView>>),
    FetchTxsWithCycles(Request<Vec<ProposalShortId>, FetchTxsWithCyclesResult>),
    GetTxPoolInfo(Request<(), TxPoolInfo>),
    FetchTxRPC(Request<ProposalShortId, Option<(bool, TransactionView)>>),
    NewUncle(Notify<UncleBlockView>),
    PlugEntry(Request<(Vec<TxEntry>, PlugTarget), ()>),
    EstimateFeeRate(Request<usize, FeeRate>),
}

#[derive(Clone)]
pub struct TxPoolController {
    sender: mpsc::Sender<Message>,
    handle: Handle,
    stop: StopHandler<()>,
}

impl Drop for TxPoolController {
    fn drop(&mut self) {
        self.stop.try_send();
    }
}

impl TxPoolController {
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    pub fn get_block_template(
        &self,
        bytes_limit: Option<u64>,
        proposals_limit: Option<u64>,
        max_version: Option<Version>,
    ) -> Result<BlockTemplateResult, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call((bytes_limit, proposals_limit, max_version), responder);
        sender
            .try_send(Message::BlockTemplate(request))
            .map_err(|e| {
                let (_m, e) = handle_try_send_error(e);
                e
            })?;
        response.recv().map_err(Into::into)
    }

    pub fn notify_new_uncle(&self, uncle: UncleBlockView) -> Result<(), FailureError> {
        let mut sender = self.sender.clone();
        let notify = Notify::notify(uncle);
        sender.try_send(Message::NewUncle(notify)).map_err(|e| {
            let (_m, e) = handle_try_send_error(e);
            e.into()
        })
    }

    pub fn update_tx_pool_for_reorg(
        &self,
        detached_blocks: VecDeque<BlockView>,
        attached_blocks: VecDeque<BlockView>,
        detached_proposal_id: HashSet<ProposalShortId>,
        snapshot: Arc<Snapshot>,
    ) -> Result<(), FailureError> {
        let mut sender = self.sender.clone();
        let notify = Notify::notify((
            detached_blocks,
            attached_blocks,
            detached_proposal_id,
            snapshot,
        ));
        sender.try_send(Message::ChainReorg(notify)).map_err(|e| {
            let (_m, e) = handle_try_send_error(e);
            e.into()
        })
    }

    pub fn submit_txs(&self, txs: Vec<TransactionView>) -> Result<SubmitTxsResult, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call(txs, responder);
        sender.try_send(Message::SubmitTxs(request)).map_err(|e| {
            let (_m, e) = handle_try_send_error(e);
            e
        })?;
        response.recv().map_err(Into::into)
    }

    pub fn plug_entry(
        &self,
        entries: Vec<TxEntry>,
        target: PlugTarget,
    ) -> Result<(), FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call((entries, target), responder);
        sender.try_send(Message::PlugEntry(request)).map_err(|e| {
            let (_m, e) = handle_try_send_error(e);
            e
        })?;
        response.recv().map_err(Into::into)
    }

    pub fn notify_txs(
        &self,
        txs: Vec<TransactionView>,
        callback: NotifyTxsCallback,
    ) -> Result<(), FailureError> {
        let mut sender = self.sender.clone();
        let notify = Notify::notify((txs, callback));
        sender.try_send(Message::NotifyTxs(notify)).map_err(|e| {
            let (_m, e) = handle_try_send_error(e);
            e.into()
        })
    }

    pub fn get_tx_pool_info(&self) -> Result<TxPoolInfo, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call((), responder);
        sender
            .try_send(Message::GetTxPoolInfo(request))
            .map_err(|e| {
                let (_m, e) = handle_try_send_error(e);
                e
            })?;
        response.recv().map_err(Into::into)
    }

    pub fn fresh_proposals_filter(
        &self,
        proposals: Vec<ProposalShortId>,
    ) -> Result<Vec<ProposalShortId>, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call(proposals, responder);
        sender
            .try_send(Message::FreshProposalsFilter(request))
            .map_err(|e| {
                let (_m, e) = handle_try_send_error(e);
                e
            })?;
        response.recv().map_err(Into::into)
    }

    pub fn fetch_tx_for_rpc(&self, id: ProposalShortId) -> Result<FetchTxRPCResult, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call(id, responder);
        sender.try_send(Message::FetchTxRPC(request)).map_err(|e| {
            let (_m, e) = handle_try_send_error(e);
            e
        })?;
        response.recv().map_err(Into::into)
    }

    pub fn fetch_txs(
        &self,
        short_ids: Vec<ProposalShortId>,
    ) -> Result<HashMap<ProposalShortId, TransactionView>, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call(short_ids, responder);
        sender.try_send(Message::FetchTxs(request)).map_err(|e| {
            let (_m, e) = handle_try_send_error(e);
            e
        })?;
        response.recv().map_err(Into::into)
    }

    pub fn fetch_txs_with_cycles(
        &self,
        short_ids: Vec<ProposalShortId>,
    ) -> Result<FetchTxsWithCyclesResult, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call(short_ids, responder);
        sender
            .try_send(Message::FetchTxsWithCycles(request))
            .map_err(|e| {
                let (_m, e) = handle_try_send_error(e);
                e
            })?;
        response.recv().map_err(Into::into)
    }

    pub fn estimate_fee_rate(&self, expect_confirm_blocks: usize) -> Result<FeeRate, FailureError> {
        let mut sender = self.sender.clone();
        let (responder, response) = crossbeam_channel::bounded(1);
        let request = Request::call(expect_confirm_blocks, responder);
        sender
            .try_send(Message::EstimateFeeRate(request))
            .map_err(|e| {
                let (_m, e) = handle_try_send_error(e);
                e
            })?;
        response.recv().map_err(Into::into)
    }
}

pub struct TxPoolServiceBuilder {
    service: Option<TxPoolService>,
}

impl TxPoolServiceBuilder {
    pub fn new(
        tx_pool_config: TxPoolConfig,
        snapshot: Arc<Snapshot>,
        block_assembler_config: Option<BlockAssemblerConfig>,
        txs_verify_cache: Arc<RwLock<TxVerifyCache>>,
        snapshot_mgr: Arc<SnapshotMgr>,
    ) -> TxPoolServiceBuilder {
        let last_txs_updated_at = Arc::new(AtomicU64::new(0));
        let tx_pool = TxPool::new(tx_pool_config, snapshot, Arc::clone(&last_txs_updated_at));
        let block_assembler = block_assembler_config.map(BlockAssembler::new);

        TxPoolServiceBuilder {
            service: Some(TxPoolService::new(
                tx_pool,
                block_assembler,
                txs_verify_cache,
                last_txs_updated_at,
                snapshot_mgr,
            )),
        }
    }

    pub fn start(mut self) -> TxPoolController {
        let (sender, mut receiver) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
        let (signal_sender, mut signal_receiver) = oneshot::channel();

        let service = self.service.take().expect("tx pool service start once");
        let server = move |handle: Handle| async move {
            loop {
                tokio::select! {
                    Some(message) = receiver.recv() => {
                        let service_clone = service.clone();
                        handle.spawn(process(service_clone, message));
                    },
                    _ = &mut signal_receiver => break,
                    else => break,
                }
            }
        };
        let (handle, thread) = new_runtime(server);
        let stop = StopHandler::new(SignalSender::Tokio(signal_sender), thread);
        TxPoolController {
            sender,
            handle,
            stop,
        }
    }
}

#[derive(Clone)]
pub struct TxPoolService {
    pub(crate) tx_pool: Arc<RwLock<TxPool>>,
    pub(crate) tx_pool_config: Arc<TxPoolConfig>,
    pub(crate) block_assembler: Option<BlockAssembler>,
    pub(crate) txs_verify_cache: Arc<RwLock<TxVerifyCache>>,
    pub(crate) last_txs_updated_at: Arc<AtomicU64>,
    snapshot_mgr: Arc<SnapshotMgr>,
}

impl TxPoolService {
    pub fn new(
        tx_pool: TxPool,
        block_assembler: Option<BlockAssembler>,
        txs_verify_cache: Arc<RwLock<TxVerifyCache>>,
        last_txs_updated_at: Arc<AtomicU64>,
        snapshot_mgr: Arc<SnapshotMgr>,
    ) -> Self {
        let tx_pool_config = Arc::new(tx_pool.config);
        Self {
            tx_pool: Arc::new(RwLock::new(tx_pool)),
            tx_pool_config,
            block_assembler,
            txs_verify_cache,
            last_txs_updated_at,
            snapshot_mgr,
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snapshot_mgr.load())
    }
}

#[allow(clippy::cognitive_complexity)]
async fn process(service: TxPoolService, message: Message) {
    match message {
        Message::GetTxPoolInfo(Request { responder, .. }) => {
            let info = service.tx_pool.read().await.info();
            if let Err(e) = responder.send(info) {
                error!("responder send get_tx_pool_info failed {:?}", e);
            };
        }
        Message::BlockTemplate(Request {
            responder,
            arguments: (bytes_limit, proposals_limit, max_version),
        }) => {
            let block_template_result = service
                .get_block_template(bytes_limit, proposals_limit, max_version)
                .await;
            if let Err(e) = responder.send(block_template_result) {
                error!("responder send block_template_result failed {:?}", e);
            };
        }
        Message::SubmitTxs(Request {
            responder,
            arguments: txs,
        }) => {
            let submit_txs_result = service.process_txs(txs).await;
            if let Err(e) = responder.send(submit_txs_result) {
                error!("responder send submit_txs_result failed {:?}", e);
            };
        }
        Message::NotifyTxs(Notify {
            arguments: (txs, callback),
        }) => {
            let submit_txs_result = service.process_txs(txs).await;
            if let Some(call) = callback {
                call(submit_txs_result)
            };
        }
        Message::FreshProposalsFilter(Request {
            responder,
            arguments: mut proposals,
        }) => {
            let tx_pool = service.tx_pool.read().await;
            proposals.retain(|id| !tx_pool.contains_proposal_id(&id));
            if let Err(e) = responder.send(proposals) {
                error!("responder send fresh_proposals_filter failed {:?}", e);
            };
        }
        Message::FetchTxRPC(Request {
            responder,
            arguments: id,
        }) => {
            let tx_pool = service.tx_pool.read().await;
            let tx = tx_pool
                .proposed()
                .get(&id)
                .map(|entry| (true, entry.transaction.clone()))
                .or_else(|| tx_pool.get_tx_without_conflict(&id).map(|tx| (false, tx)));
            if let Err(e) = responder.send(tx) {
                error!("responder send fetch_tx_for_rpc failed {:?}", e)
            };
        }
        Message::FetchTxs(Request {
            responder,
            arguments: short_ids,
        }) => {
            let tx_pool = service.tx_pool.read().await;
            let txs = short_ids
                .into_iter()
                .filter_map(|short_id| {
                    if let Some(tx) = tx_pool.get_tx_from_pool_or_store(&short_id) {
                        Some((short_id, tx))
                    } else {
                        None
                    }
                })
                .collect();
            if let Err(e) = responder.send(txs) {
                error!("responder send fetch_txs failed {:?}", e);
            };
        }
        Message::FetchTxsWithCycles(Request {
            responder,
            arguments: short_ids,
        }) => {
            let tx_pool = service.tx_pool.read().await;
            let txs = short_ids
                .into_iter()
                .filter_map(|short_id| {
                    tx_pool
                        .get_tx_with_cycles(&short_id)
                        .and_then(|(tx, cycles)| cycles.map(|cycles| (short_id, (tx, cycles))))
                })
                .collect();
            if let Err(e) = responder.send(txs) {
                error!("responder send fetch_txs_with_cycles failed {:?}", e);
            };
        }
        Message::ChainReorg(Notify {
            arguments: (detached_blocks, attached_blocks, detached_proposal_id, snapshot),
        }) => {
            service
                .update_tx_pool_for_reorg(
                    detached_blocks,
                    attached_blocks,
                    detached_proposal_id,
                    snapshot,
                )
                .await
        }
        Message::NewUncle(Notify { arguments: uncle }) => {
            if service.block_assembler.is_some() {
                let block_assembler = service.block_assembler.clone().unwrap();
                block_assembler.candidate_uncles.lock().await.insert(uncle);
                block_assembler
                    .last_uncles_updated_at
                    .store(unix_time_as_millis(), Ordering::SeqCst);
            }
        }
        Message::PlugEntry(Request {
            responder,
            arguments: (entries, target),
        }) => {
            let mut tx_pool = service.tx_pool.write().await;
            match target {
                PlugTarget::Pending => {
                    for entry in entries {
                        if let Err(err) = tx_pool.add_pending(entry) {
                            error!("plug entry error {}", err);
                        }
                    }
                }
                PlugTarget::Proposed => {
                    for entry in entries {
                        if let Err(err) = tx_pool.add_proposed(entry) {
                            error!("plug entry error {}", err);
                        }
                    }
                }
            };
            if let Err(e) = responder.send(()) {
                error!("responder send plug_entry failed {:?}", e);
            };
        }
        Message::EstimateFeeRate(Request {
            responder,
            arguments: expect_confirm_blocks,
        }) => {
            let tx_pool = service.tx_pool.read().await;
            let fee_rate = tx_pool.fee_estimator.estimate(expect_confirm_blocks);
            if let Err(e) = responder.send(fee_rate) {
                error!("responder send estimate_fee_rate failed {:?}", e)
            };
        }
    }
}
