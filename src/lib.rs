#![warn(
    clippy::all,
    clippy::pedantic,
    rust_2018_idioms,
    missing_docs,
    clippy::missing_docs_in_private_items,
    clippy::panic
)]
#![allow(clippy::module_name_repetitions)]
// Temporary
#![allow(clippy::cast_possible_truncation)]
// Temporary, should be disabled after the completion of the NodeImplementation refactor
#![allow(clippy::type_complexity)]
//! Provides a generic rust implementation of the `HotShot` BFT protocol
//!
//! See the [protocol documentation](https://github.com/EspressoSystems/hotshot-spec) for a protocol description.

// Documentation module
#[cfg(feature = "docs")]
pub mod documentation;

/// Data availability support
// pub mod da;
/// Contains structures and functions for committee election
pub mod certificate;
#[cfg(any(feature = "demo"))]
pub mod demos;
/// Contains traits consumed by [`HotShot`]
pub mod traits;
/// Contains types used by the crate
pub mod types;

pub mod tasks;

use crate::{
    certificate::QuorumCertificate,
    traits::{NodeImplementation, Storage},
    types::{Event, HotShotHandle},
};
use async_compatibility_layer::{
    art::{async_sleep, async_spawn, async_spawn_local},
    async_primitives::{broadcast::BroadcastSender, subscribable_rwlock::SubscribableRwLock},
    channel::{unbounded, UnboundedReceiver, UnboundedSender},
};
use async_lock::{Mutex, RwLock, RwLockUpgradableReadGuard, RwLockWriteGuard};
use async_trait::async_trait;
use bincode::Options;
use commit::{Commitment, Committable};

use hotshot_consensus::{
    BlockStore, Consensus, ConsensusApi, ConsensusLeader, ConsensusMetrics, ConsensusNextLeader,
    DALeader, DAMember, NextValidatingLeader, Replica, SendToTasks, SequencingReplica,
    ValidatingLeader, View, ViewInner, ViewQueue,
};
use hotshot_types::certificate::DACertificate;

use hotshot_types::data::CommitmentProposal;
use hotshot_types::data::{DAProposal, DeltasType, SequencingLeaf};
use hotshot_types::traits::election::CommitteeExchangeType;
use hotshot_types::traits::election::QuorumExchangeType;
use hotshot_types::traits::network::CommunicationChannel;
use hotshot_types::{data::ProposalType, traits::election::ConsensusExchange};
use hotshot_types::{
    data::{LeafType, ValidatingLeaf, ValidatingProposal},
    error::StorageSnafu,
    message::{
        ConsensusMessage, DataMessage, InternalTrigger, Message, MessageKind,
        ProcessedConsensusMessage,
    },
    traits::{
        election::SignedCertificate,
        metrics::Metrics,
        network::{NetworkError, TransmitType},
        node_implementation::NodeType,
        signature_key::SignatureKey,
        state::{ConsensusTime, ConsensusType, SequencingConsensus, ValidatingConsensus},
        storage::StoredView,
        State,
    },
    vote::{DAVote, QuorumVote, VoteType},
    HotShotConfig,
};
use hotshot_utils::bincode::bincode_opts;
use snafu::ResultExt;
use std::{
    collections::{BTreeMap, HashMap},
    marker::PhantomData,
    num::NonZeroUsize,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};
use tracing::{debug, error, info, instrument, trace, warn};
// -- Rexports
// External
/// Reexport rand crate
pub use rand;
// Internal
/// Reexport error type
pub use hotshot_types::error::HotShotError;

/// Length, in bytes, of a 512 bit hash
pub const H_512: usize = 64;
/// Length, in bytes, of a 256 bit hash
pub const H_256: usize = 32;

/// Holds the state needed to participate in `HotShot` consensus
pub struct HotShotInner<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// The public key of this node
    public_key: TYPES::SignatureKey,

    /// The private key of this node
    private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,

    /// Configuration items for this hotshot instance
    config: HotShotConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,

    /// Networking interface for this hotshot instance
    // networking: I::Networking,

    /// This `HotShot` instance's storage backend
    storage: I::Storage,

    /// This `HotShot` instance's way to interact with the nodes needed to form a quorum
    pub quorum_exchange: Arc<I::QuorumExchange>,

    /// This `HotShot` instance's interaction with the DA committee to form a DA certificate.
    pub committee_exchange: Arc<I::CommitteeExchange>,

    /// Sender for [`Event`]s
    event_sender: RwLock<Option<BroadcastSender<Event<TYPES, I::Leaf>>>>,

    /// Senders to the background tasks.
    background_task_handle: tasks::TaskHandle<TYPES>,

    /// a reference to the metrics that the implementor is using.
    metrics: Box<dyn Metrics>,
}

/// Thread safe, shared view of a `HotShot`
#[derive(Clone)]
pub struct HotShot<CONSENSUS: ConsensusType, TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// Handle to internal hotshot implementation
    inner: Arc<HotShotInner<TYPES, I>>,

    /// Transactions
    /// (this is shared btwn hotshot and `Consensus`)
    transactions:
        Arc<SubscribableRwLock<HashMap<Commitment<TYPES::Transaction>, TYPES::Transaction>>>,

    /// The hotstuff implementation
    hotstuff: Arc<RwLock<Consensus<TYPES, I::Leaf>>>,

    /// for sending/recv-ing things with the DA member task
    member_channel_map: Arc<RwLock<SendToTasks<TYPES, I>>>,

    /// for sending/recv-ing things with the replica task
    replica_channel_map: Arc<RwLock<SendToTasks<TYPES, I>>>,

    /// for sending/recv-ing things with the next leader task
    next_leader_channel_map: Arc<RwLock<SendToTasks<TYPES, I>>>,

    /// for sending/recv-ing things to the da leader
    da_leader_channel_map: Arc<RwLock<SendToTasks<TYPES, I>>>,

    /// for sending messages to network lookup task
    send_network_lookup: UnboundedSender<Option<TYPES::Time>>,

    /// for receiving messages in the network lookup task
    recv_network_lookup: Arc<Mutex<UnboundedReceiver<Option<TYPES::Time>>>>,

    /// uid for instrumentation
    id: u64,

    /// Phantom data for consensus type
    _pd: PhantomData<CONSENSUS>,
}

impl<TYPES: NodeType, I: NodeImplementation<TYPES>> HotShot<TYPES::ConsensusType, TYPES, I> {
    /// Creates a new hotshot with the given configuration options and sets it up with the given
    /// genesis block
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip(
        private_key,
        storage,
        quorum_exchange,
        committee_exchange,
        initializer,
        metrics
    ))]
    pub async fn new(
        public_key: TYPES::SignatureKey,
        private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
        nonce: u64,
        config: HotShotConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
        // networking: I::Networking,
        storage: I::Storage,
        quorum_exchange: I::QuorumExchange,
        committee_exchange: I::CommitteeExchange,
        initializer: HotShotInitializer<TYPES, I::Leaf>,
        metrics: Box<dyn Metrics>,
    ) -> Result<Self, HotShotError<TYPES>> {
        info!("Creating a new hotshot");
        let inner: Arc<HotShotInner<TYPES, I>> = Arc::new(HotShotInner {
            public_key,
            private_key,
            config,
            // networking,
            storage,
            quorum_exchange: Arc::new(quorum_exchange),
            committee_exchange: Arc::new(committee_exchange),
            event_sender: RwLock::default(),
            background_task_handle: tasks::TaskHandle::default(),
            metrics,
        });

        let anchored_leaf = initializer.inner;

        // insert to storage
        inner
            .storage
            .append(vec![anchored_leaf.clone().into()])
            .await
            .context(StorageSnafu)?;

        // insert genesis (or latest block) to state map
        let mut state_map = BTreeMap::default();
        state_map.insert(
            anchored_leaf.get_view_number(),
            View {
                view_inner: ViewInner::Leaf {
                    leaf: anchored_leaf.commit(),
                },
            },
        );

        let mut saved_leaves = HashMap::new();
        let mut saved_blocks = BlockStore::default();
        saved_leaves.insert(anchored_leaf.commit(), anchored_leaf.clone());
        if let Ok(block) = anchored_leaf.get_deltas().try_resolve() {
            saved_blocks.insert(block);
        }

        let start_view = anchored_leaf.get_view_number();

        let hotstuff = Consensus {
            state_map,
            cur_view: start_view,
            last_decided_view: anchored_leaf.get_view_number(),
            transactions: Arc::default(),
            saved_leaves,
            saved_blocks,
            // TODO this is incorrect
            // https://github.com/EspressoSystems/HotShot/issues/560
            locked_view: anchored_leaf.get_view_number(),
            high_qc: anchored_leaf.get_justify_qc(),

            metrics: Arc::new(ConsensusMetrics::new(
                &*inner.metrics.subgroup("consensus".to_string()),
            )),
            invalid_qc: 0,
        };
        let hotstuff = Arc::new(RwLock::new(hotstuff));
        let txns = hotstuff.read().await.get_transactions();

        let (send_network_lookup, recv_network_lookup) = unbounded();

        Ok(Self {
            id: nonce,
            inner,
            transactions: txns,
            hotstuff,
            member_channel_map: Arc::new(RwLock::new(SendToTasks::new(start_view))),
            replica_channel_map: Arc::new(RwLock::new(SendToTasks::new(start_view))),
            next_leader_channel_map: Arc::new(RwLock::new(SendToTasks::new(start_view))),
            da_leader_channel_map: Arc::new(RwLock::new(SendToTasks::new(start_view))),
            send_network_lookup,
            recv_network_lookup: Arc::new(Mutex::new(recv_network_lookup)),
            _pd: PhantomData,
        })
    }

    /// Marks a given view number as timed out. This should be called a fixed period after a round is started.
    ///
    /// If the round has already ended then this function will essentially be a no-op. Otherwise `run_round` will return shortly after this function is called.
    /// # Panics
    /// Panics if the current view is not in the channel map
    #[instrument(
        skip_all,
        fields(id = self.id, view = *current_view),
        name = "Timeout consensus tasks",
        level = "warn"
    )]
    pub async fn timeout_view(
        &self,
        current_view: TYPES::Time,
        send_replica: UnboundedSender<ProcessedConsensusMessage<TYPES, I>>,
        send_next_leader: Option<UnboundedSender<ProcessedConsensusMessage<TYPES, I>>>,
    ) {
        let msg = ProcessedConsensusMessage::<TYPES, I>::InternalTrigger(InternalTrigger::Timeout(
            current_view,
        ));
        if let Some(chan) = send_next_leader {
            if chan.send(msg.clone()).await.is_err() {
                warn!("Error timing out next leader task");
            }
        };
        // NOTE this should always exist
        if send_replica.send(msg).await.is_err() {
            warn!("Error timing out replica task");
        };
    }

    /// time out a da view
    #[instrument(
        skip_all,
        fields(id = self.id, view = *current_view),
        name = "Timeout consensus tasks",
        level = "warn"
    )]
    pub async fn timeout_da_view(
        &self,
        current_view: TYPES::Time,
        send_da_member: UnboundedSender<ProcessedConsensusMessage<TYPES, I>>,
        send_da_leader: Option<UnboundedSender<ProcessedConsensusMessage<TYPES, I>>>,
    ) {
        let msg = ProcessedConsensusMessage::<TYPES, I>::InternalTrigger(InternalTrigger::Timeout(
            current_view,
        ));
        if let Some(chan) = send_da_leader {
            if chan.send(msg.clone()).await.is_err() {
                warn!("Error timing out next leader task");
            }
        };
        // NOTE this should always exist
        if send_da_member.send(msg).await.is_err() {
            warn!("Error timing out replica task");
        };
    }

    /// Publishes a transaction to the network
    ///
    /// # Errors
    ///
    /// Will generate an error if an underlying network error occurs
    #[instrument(skip(self), err)]
    pub async fn publish_transaction_async(
        &self,
        transaction: TYPES::Transaction,
    ) -> Result<(), HotShotError<TYPES>> {
        // Add the transaction to our own queue first
        trace!("Adding transaction to our own queue");
        // Wrap up a message
        // TODO place a view number here that makes sense
        // we haven't worked out how this will work yet
        let message = DataMessage::SubmitTransaction(transaction, TYPES::Time::new(0));

        let api = self.clone();
        async_spawn(async move {
            let _result = api.send_broadcast_message(message).await.is_err();
        });
        Ok(())
    }

    /// Returns a copy of the state
    ///
    /// # Panics
    ///
    /// Panics if internal state for consensus is inconsistent
    pub async fn get_state(&self) -> <I::Leaf as LeafType>::StateCommitmentType {
        self.hotstuff.read().await.get_decided_leaf().get_state()
    }

    /// Returns a copy of the last decided leaf
    /// # Panics
    /// Panics if internal state for consensus is inconsistent
    pub async fn get_decided_leaf(&self) -> I::Leaf {
        self.hotstuff.read().await.get_decided_leaf()
    }

    /// Initializes a new hotshot and does the work of setting up all the background tasks
    ///
    /// Assumes networking implementation is already primed.
    ///
    /// Underlying `HotShot` instance starts out paused, and must be unpaused
    ///
    /// Upon encountering an unrecoverable error, such as a failure to send to a broadcast channel,
    /// the `HotShot` instance will log the error and shut down.
    ///
    /// # Errors
    ///
    /// Will return an error when the storage failed to insert the first `QuorumCertificate`
    #[allow(clippy::too_many_arguments)]
    pub async fn init(
        public_key: TYPES::SignatureKey,
        private_key: <TYPES::SignatureKey as SignatureKey>::PrivateKey,
        node_id: u64,
        config: HotShotConfig<TYPES::SignatureKey, TYPES::ElectionConfigType>,
        storage: I::Storage,
        quorum_exchange: I::QuorumExchange,
        committee_exchange: I::CommitteeExchange,
        initializer: HotShotInitializer<TYPES, I::Leaf>,
        metrics: Box<dyn Metrics>,
    ) -> Result<HotShotHandle<TYPES, I>, HotShotError<TYPES>>
    where
        HotShot<TYPES::ConsensusType, TYPES, I>: ViewRunner<TYPES, I>,
    {
        // Save a clone of the storage for the handle
        let hotshot = Self::new(
            public_key,
            private_key,
            node_id,
            config,
            storage,
            quorum_exchange,
            committee_exchange,
            initializer,
            metrics,
        )
        .await?;
        let handle = tasks::spawn_all(&hotshot).await;

        Ok(handle)
    }

    /// Send a broadcast message.
    ///
    /// This is an alias for `hotshot.inner.networking.broadcast_message(msg.into())`.
    ///
    /// # Errors
    ///
    /// Will return any errors that the underlying `broadcast_message` can return.
    pub async fn send_broadcast_message(
        &self,
        kind: impl Into<MessageKind<TYPES, I>>,
    ) -> std::result::Result<(), NetworkError> {
        let inner = self.inner.clone();
        let pk = self.inner.public_key.clone();
        let kind = kind.into();
        async_spawn_local(async move {
            if inner
                .quorum_exchange
                .network()
                .broadcast_message(
                    Message { sender: pk, kind },
                    // TODO this is morally wrong
                    &inner.quorum_exchange.membership().clone(),
                )
                .await
                .is_err()
            {
                warn!("Failed to broadcast message");
            };
        });
        Ok(())
    }

    /// Send a direct message to a given recipient.
    ///
    /// This is an alias for `hotshot.inner.networking.message_node(msg.into(), recipient)`.
    ///
    /// # Errors
    ///
    /// Will return any errors that the underlying `message_node` can return.
    pub async fn send_direct_message(
        &self,
        kind: impl Into<MessageKind<TYPES, I>>,
        recipient: TYPES::SignatureKey,
    ) -> std::result::Result<(), NetworkError> {
        self.inner
            .quorum_exchange
            .network()
            .direct_message(
                Message {
                    sender: self.inner.public_key.clone(),
                    kind: kind.into(),
                },
                recipient,
            )
            .await?;
        Ok(())
    }

    /// Handle an incoming [`ConsensusMessage`] that was broadcasted on the network.
    #[instrument(
        skip(self),
        name = "Handle broadcast consensus message",
        level = "error"
    )]
    async fn handle_broadcast_consensus_message(
        &self,
        msg: ConsensusMessage<TYPES, I>,
        sender: TYPES::SignatureKey,
    ) {
        // TODO validate incoming data message based on sender signature key
        // <github.com/ExpressoSystems/HotShot/issues/418>
        let msg_time = msg.view_number();

        match msg {
            // this is ONLY intended for replica
            ConsensusMessage::Proposal(_) => {
                let channel_map = self.replica_channel_map.upgradable_read().await;

                // skip if the proposal is stale
                if msg_time < channel_map.cur_view {
                    warn!("Throwing away proposal for view number: {:?}", msg_time);
                    return;
                }

                let chan: ViewQueue<TYPES, I> =
                    Self::create_or_obtain_chan_from_read(msg_time, channel_map).await;

                if !chan.has_received_proposal.swap(true, Ordering::Relaxed)
                    && chan
                        .sender_chan
                        .send(ProcessedConsensusMessage::new(msg, sender))
                        .await
                        .is_err()
                {
                    warn!("Failed to send to next leader!");
                }
            }
            ConsensusMessage::InternalTrigger(_) => {
                warn!("Received an internal trigger. This shouldn't be possible.");
            }
            ConsensusMessage::Vote(_) => {
                warn!("Received a broadcast for a vote message. This shouldn't be possible.");
            }
            ConsensusMessage::DAVote(_) => {
                warn!("Received a broadcast for a vote message. This shouldn't be possible.");
            }
            ConsensusMessage::DAProposal(_) => {
                let channel_map = self.member_channel_map.upgradable_read().await;

                // skip if the proposal is stale
                if msg_time < channel_map.cur_view {
                    warn!("Throwing away DA proposal for view number: {:?}", msg_time);
                    return;
                }

                let chan: ViewQueue<TYPES, I> =
                    Self::create_or_obtain_chan_from_read(msg_time, channel_map).await;

                if !chan.has_received_proposal.swap(true, Ordering::Relaxed)
                    && chan
                        .sender_chan
                        .send(ProcessedConsensusMessage::new(msg, sender))
                        .await
                        .is_err()
                {
                    warn!("Failed to send to next leader!");
                }
            }
        };
    }

    /// decide which handler to call based on the message variant and `transmit_type`
    async fn handle_message(&self, item: Message<TYPES, I>, transmit_type: TransmitType) {
        match (item.kind, transmit_type) {
            (MessageKind::Consensus(msg), TransmitType::Broadcast) => {
                self.handle_broadcast_consensus_message(msg, item.sender)
                    .await;
            }
            (MessageKind::Consensus(msg), TransmitType::Direct) => {
                self.handle_direct_consensus_message(msg, item.sender).await;
            }
            (MessageKind::Data(msg), TransmitType::Broadcast) => {
                self.handle_broadcast_data_message(msg, item.sender).await;
            }
            (MessageKind::Data(msg), TransmitType::Direct) => {
                self.handle_direct_data_message(msg, item.sender).await;
            }
        };
    }

    /// Handle an incoming [`ConsensusMessage`] directed at this node.
    #[instrument(skip(self), name = "Handle direct consensus message", level = "error")]
    async fn handle_direct_consensus_message(
        &self,
        msg: ConsensusMessage<TYPES, I>,
        sender: TYPES::SignatureKey,
    ) {
        // We can only recv from a replicas
        // replicas should only send votes or if they timed out, timeouts
        match msg {
            ConsensusMessage::Proposal(_) | ConsensusMessage::InternalTrigger(_) => {
                warn!("Received a direct message for a proposal. This shouldn't be possible.");
            }
            // this is ONLY intended for next leader
            c @ ConsensusMessage::Vote(_) => {
                let msg_time = c.view_number();

                let channel_map = self.next_leader_channel_map.upgradable_read().await;

                // check if
                // - is in fact, actually is the next leader
                // - the message is not stale
                let is_leader = self.inner.clone().quorum_exchange.is_leader(msg_time + 1);
                if !is_leader || msg_time < channel_map.cur_view {
                    warn!(
                        "Throwing away VoteType<TYPES>message for view number: {:?}",
                        msg_time
                    );
                    return;
                }

                let chan = Self::create_or_obtain_chan_from_read(msg_time, channel_map).await;

                if chan
                    .sender_chan
                    .send(ProcessedConsensusMessage::new(c, sender))
                    .await
                    .is_err()
                {
                    error!("Failed to send to next leader!");
                }
            }
            c @ ConsensusMessage::DAVote(_) => {
                let msg_time = c.view_number();

                let channel_map = self.da_leader_channel_map.upgradable_read().await;

                // check if
                // - is in fact, actually is the next leader
                // - the message is not stale
                let is_leader = self.inner.clone().committee_exchange.is_leader(msg_time);
                if !is_leader || msg_time < channel_map.cur_view {
                    warn!(
                        "Throwing away VoteType<TYPES>message for view number: {:?}, Channel cur view: {:?}",
                        msg_time,
                        channel_map.cur_view,
                    );
                    return;
                }

                let chan = Self::create_or_obtain_chan_from_read(msg_time, channel_map).await;

                if chan
                    .sender_chan
                    .send(ProcessedConsensusMessage::new(c, sender))
                    .await
                    .is_err()
                {
                    error!("Failed to send to next leader!");
                }
            }
            ConsensusMessage::DAProposal(_) => todo!(),
        }
    }

    /// Handle an incoming [`DataMessage`] that was broadcasted on the network
    async fn handle_broadcast_data_message(
        &self,
        msg: DataMessage<TYPES>,
        _sender: TYPES::SignatureKey,
    ) {
        // TODO validate incoming broadcast message based on sender signature key
        match msg {
            DataMessage::SubmitTransaction(transaction, _view_number) => {
                let size = bincode_opts().serialized_size(&transaction).unwrap_or(0);

                // The API contract requires the hash to be unique
                // so we can assume entry == incoming txn
                // even if eq not satisfied
                // so insert is an idempotent operation
                let mut new = false;
                self.transactions
                    .modify(|txns| {
                        new = txns.insert(transaction.commit(), transaction).is_none();
                    })
                    .await;

                if new {
                    // If this is a new transaction, update metrics.
                    let consensus = self.hotstuff.read().await;
                    consensus.metrics.outstanding_transactions.update(1);
                    consensus
                        .metrics
                        .outstanding_transactions_memory_size
                        .update(i64::try_from(size).unwrap_or(i64::MAX));
                }
            }
        }
    }

    /// Handle an incoming [`DataMessage`] that directed at this node
    #[allow(clippy::unused_async)] // async for API compatibility reasons
    async fn handle_direct_data_message(
        &self,
        msg: DataMessage<TYPES>,
        _sender: TYPES::SignatureKey,
    ) {
        debug!(?msg, "Incoming direct data message");
        match msg {
            DataMessage::SubmitTransaction(_, _) => {
                // Log exceptional situation and proceed
                warn!(?msg, "Broadcast message received over direct channel");
            }
        }
    }

    /// return the timeout for a view for `self`
    #[must_use]
    pub fn get_next_view_timeout(&self) -> u64 {
        self.inner.config.next_view_timeout
    }

    /// given a view number and a upgradable read lock on a channel map, inserts entry into map if it
    /// doesn't exist, or creates entry. Then returns a clone of the entry
    pub async fn create_or_obtain_chan_from_read(
        view_num: TYPES::Time,
        channel_map: RwLockUpgradableReadGuard<'_, SendToTasks<TYPES, I>>,
    ) -> ViewQueue<TYPES, I> {
        // check if we have the entry
        // if we don't, insert
        if let Some(vq) = channel_map.channel_map.get(&view_num) {
            vq.clone()
        } else {
            let mut channel_map =
                RwLockUpgradableReadGuard::<'_, SendToTasks<TYPES, I>>::upgrade(channel_map).await;
            let new_view_queue = ViewQueue::default();
            let vq = new_view_queue.clone();
            // NOTE: the read lock is held until all other read locks are DROPPED and
            // the read lock may be turned into a write lock.
            // This means that the `channel_map` will not change. So we don't need
            // to check again to see if a channel was added

            channel_map.channel_map.insert(view_num, new_view_queue);
            vq
        }
    }

    /// given a view number and a write lock on a channel map, inserts entry into map if it
    /// doesn't exist, or creates entry. Then returns a clone of the entry
    #[allow(clippy::unused_async)] // async for API compatibility reasons
    pub async fn create_or_obtain_chan_from_write(
        view_num: TYPES::Time,
        mut channel_map: RwLockWriteGuard<'_, SendToTasks<TYPES, I>>,
    ) -> ViewQueue<TYPES, I> {
        channel_map.channel_map.entry(view_num).or_default().clone()
    }
}

/// A view runner implemented by [HotShot] for different types of consensus.
#[async_trait]
pub trait ViewRunner<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// Executes one view of consensus
    async fn run_view(hotshot: HotShot<TYPES::ConsensusType, TYPES, I>) -> Result<(), ()>;
}

#[async_trait]
impl<
        TYPES: NodeType<ConsensusType = ValidatingConsensus>,
        I: NodeImplementation<TYPES, Leaf = ValidatingLeaf<TYPES>>,
    > ViewRunner<TYPES, I> for HotShot<ValidatingConsensus, TYPES, I>
where
    I::QuorumExchange: ConsensusExchange<
            TYPES,
            I::Leaf,
            Message<TYPES, I>,
            Proposal = ValidatingProposal<TYPES, I::Leaf>,
            Vote = QuorumVote<TYPES, I::Leaf>,
            Certificate = QuorumCertificate<TYPES, I::Leaf>,
            Commitment = ValidatingLeaf<TYPES>,
        > + QuorumExchangeType<TYPES, I::Leaf, Message<TYPES, I>>,
{
    #[instrument(skip(hotshot), fields(id = hotshot.id), name = "Validating View Runner Task", level = "error")]
    async fn run_view(hotshot: HotShot<TYPES::ConsensusType, TYPES, I>) -> Result<(), ()> {
        let c_api = HotShotConsensusApi {
            inner: hotshot.inner.clone(),
        };
        let start = Instant::now();
        let metrics = Arc::clone(&hotshot.hotstuff.read().await.metrics);

        // do book keeping on channel map
        // TODO probably cleaner to separate this into a function
        // e.g. insert the view and remove the last view
        let mut send_to_replica = hotshot.replica_channel_map.write().await;
        let replica_last_view: TYPES::Time = send_to_replica.cur_view;
        // gc previous view's channel map
        send_to_replica.channel_map.remove(&replica_last_view);
        send_to_replica.cur_view += 1;
        let replica_cur_view = send_to_replica.cur_view;
        let ViewQueue {
            sender_chan: send_replica,
            receiver_chan: recv_replica,
            has_received_proposal: _,
        } = HotShot::<ValidatingConsensus, TYPES, I>::create_or_obtain_chan_from_write(
            replica_cur_view,
            send_to_replica,
        )
        .await;

        let mut send_to_next_leader = hotshot.next_leader_channel_map.write().await;
        let next_leader_last_view = send_to_next_leader.cur_view;
        // gc previous view's channel map
        send_to_next_leader
            .channel_map
            .remove(&next_leader_last_view);
        send_to_next_leader.cur_view += 1;
        let next_leader_cur_view = send_to_next_leader.cur_view;
        let (send_next_leader, recv_next_leader) = if c_api
            .inner
            .quorum_exchange
            .is_leader(next_leader_cur_view + 1)
        {
            let vq = HotShot::<ValidatingConsensus, TYPES, I>::create_or_obtain_chan_from_write(
                next_leader_cur_view,
                send_to_next_leader,
            )
            .await;
            (Some(vq.sender_chan), Some(vq.receiver_chan))
        } else {
            (None, None)
        };

        // increment consensus and start tasks

        let (cur_view, high_qc, txns) = {
            // OBTAIN write lock on consensus
            let mut consensus = hotshot.hotstuff.write().await;
            let cur_view = consensus.increment_view();
            // make sure consistent
            assert_eq!(cur_view, next_leader_cur_view);
            assert_eq!(cur_view, replica_cur_view);
            let high_qc = consensus.high_qc.clone();
            let txns = consensus.transactions.clone();
            // DROP write lock on consensus
            drop(consensus);
            (cur_view, high_qc, txns)
        };

        // notify networking to start worrying about the (`cur_view + LOOK_AHEAD`)th leader ahead of the current view
        if hotshot
            .send_network_lookup
            .send(Some(cur_view))
            .await
            .is_err()
        {
            error!("Failed to initiate network lookup");
        };

        info!("Starting tasks for View {:?}!", cur_view);
        metrics.current_view.set(*cur_view as usize);

        let mut task_handles = Vec::new();

        // replica always runs? TODO this will change once vrf integration is added
        let replica = Replica {
            id: hotshot.id,
            consensus: hotshot.hotstuff.clone(),
            proposal_collection_chan: recv_replica,
            cur_view,
            high_qc: high_qc.clone(),
            api: c_api.clone(),
            exchange: c_api.inner.quorum_exchange.clone(),
            _pd: PhantomData,
        };
        let replica_handle = async_spawn(async move {
            Replica::<HotShotConsensusApi<TYPES, I>, TYPES, I>::run_view(replica).await
        });
        task_handles.push(replica_handle);

        if c_api.inner.quorum_exchange.is_leader(cur_view) {
            let leader = ValidatingLeader {
                id: hotshot.id,
                consensus: hotshot.hotstuff.clone(),
                high_qc: high_qc.clone(),
                cur_view,
                transactions: txns,
                api: c_api.clone(),
                exchange: c_api.inner.quorum_exchange.clone(),
                _pd: PhantomData,
            };
            let leader_handle = async_spawn(async move { leader.run_view().await });
            task_handles.push(leader_handle);
        }

        if c_api.inner.quorum_exchange.is_leader(cur_view + 1) {
            let next_leader = NextValidatingLeader {
                id: hotshot.id,
                generic_qc: high_qc,
                // should be fine to unwrap here since the view numbers must be the same
                vote_collection_chan: recv_next_leader.unwrap(),
                cur_view,
                api: c_api.clone(),
                exchange: c_api.inner.quorum_exchange.clone(),
                metrics,
                _pd: PhantomData,
            };
            let next_leader_handle = async_spawn(async move {
                NextValidatingLeader::<HotShotConsensusApi<TYPES, I>, TYPES, I>::run_view(
                    next_leader,
                )
                .await
            });
            task_handles.push(next_leader_handle);
        }

        let children_finished = futures::future::join_all(task_handles);

        async_spawn({
            let next_view_timeout = hotshot.inner.config.next_view_timeout;
            let next_view_timeout = next_view_timeout;
            let hotshot: HotShot<TYPES::ConsensusType, TYPES, I> = hotshot.clone();
            async move {
                async_sleep(Duration::from_millis(next_view_timeout)).await;
                hotshot
                    .timeout_view(cur_view, send_replica, send_next_leader)
                    .await;
            }
        });

        let results = children_finished.await;

        // unwrap is fine since results must have >= 1 item(s)
        #[cfg(feature = "async-std-executor")]
        let high_qc = results
            .into_iter()
            .max_by_key(|qc: &QuorumCertificate<TYPES, ValidatingLeaf<TYPES>>| qc.view_number)
            .unwrap();
        #[cfg(feature = "tokio-executor")]
        let high_qc = results
            .into_iter()
            .filter_map(std::result::Result::ok)
            .max_by_key(|qc: &QuorumCertificate<TYPES, ValidatingLeaf<TYPES>>| qc.view_number)
            .unwrap();

        #[cfg(not(any(feature = "async-std-executor", feature = "tokio-executor")))]
        compile_error! {"Either feature \"async-std-executor\" or feature \"tokio-executor\" must be enabled for this crate."}

        let mut consensus = hotshot.hotstuff.write().await;
        consensus.high_qc = high_qc;
        consensus
            .metrics
            .view_duration
            .add_point(start.elapsed().as_secs_f64());
        c_api.send_view_finished(consensus.cur_view).await;

        info!("Returning from view {:?}!", cur_view);
        Ok(())
    }
}

#[async_trait]
impl<
        TYPES: NodeType<ConsensusType = SequencingConsensus>,
        I: NodeImplementation<TYPES, Leaf = SequencingLeaf<TYPES>>,
    > ViewRunner<TYPES, I> for HotShot<SequencingConsensus, TYPES, I>
where
    I::QuorumExchange: ConsensusExchange<
            TYPES,
            I::Leaf,
            Message<TYPES, I>,
            Proposal = CommitmentProposal<TYPES, I::Leaf>,
            Certificate = QuorumCertificate<TYPES, I::Leaf>,
            Vote = QuorumVote<TYPES, I::Leaf>,
            Commitment = SequencingLeaf<TYPES>,
        > + QuorumExchangeType<TYPES, I::Leaf, Message<TYPES, I>>,
    I::CommitteeExchange: ConsensusExchange<
            TYPES,
            I::Leaf,
            Message<TYPES, I>,
            Proposal = DAProposal<TYPES>,
            Certificate = DACertificate<TYPES>,
            Vote = DAVote<TYPES, I::Leaf>,
            Commitment = TYPES::BlockType,
        > + CommitteeExchangeType<TYPES, I::Leaf, Message<TYPES, I>>,
{
    // #[instrument]
    #[allow(clippy::too_many_lines)]
    async fn run_view(hotshot: HotShot<SequencingConsensus, TYPES, I>) -> Result<(), ()> {
        let c_api = HotShotConsensusApi {
            inner: hotshot.inner.clone(),
        };

        // Setup channel for recieving DA votes
        let mut send_to_leader = hotshot.da_leader_channel_map.write().await;
        let leader_last_view: TYPES::Time = send_to_leader.cur_view;
        send_to_leader.channel_map.remove(&leader_last_view);
        send_to_leader.cur_view += 1;
        let (send_da_vote_chan, recv_da_vote, cur_view) = {
            let mut consensus = hotshot.hotstuff.write().await;
            let cur_view = consensus.increment_view();
            let vq = HotShot::<SequencingConsensus, TYPES, I>::create_or_obtain_chan_from_write(
                cur_view,
                send_to_leader,
            )
            .await;
            (vq.sender_chan, vq.receiver_chan, cur_view)
        };

        // Set up vote collection channel for commitment proposals/votes
        let mut send_to_next_leader = hotshot.next_leader_channel_map.write().await;
        let leader_last_view: TYPES::Time = send_to_next_leader.cur_view;
        send_to_next_leader.channel_map.remove(&leader_last_view);
        send_to_next_leader.cur_view += 1;
        let (send_commitment_vote_chan, recv_commitment_vote_chan) = {
            let vq = HotShot::<SequencingConsensus, TYPES, I>::create_or_obtain_chan_from_write(
                cur_view,
                send_to_next_leader,
            )
            .await;
            (vq.sender_chan, vq.receiver_chan)
        };

        let (high_qc, txns) = {
            // OBTAIN read lock on consensus
            let consensus = hotshot.hotstuff.read().await;
            let high_qc = consensus.high_qc.clone();
            let txns = consensus.transactions.clone();
            (high_qc, txns)
        };
        let mut send_to_member = hotshot.member_channel_map.write().await;
        let member_last_view: TYPES::Time = send_to_member.cur_view;
        send_to_member.channel_map.remove(&member_last_view);
        send_to_member.cur_view += 1;
        let ViewQueue {
            sender_chan: send_member,
            receiver_chan: recv_member,
            has_received_proposal: _,
        } = HotShot::<SequencingConsensus, TYPES, I>::create_or_obtain_chan_from_write(
            send_to_member.cur_view,
            send_to_member,
        )
        .await;
        let mut send_to_replica = hotshot.replica_channel_map.write().await;
        let replica_last_view: TYPES::Time = send_to_replica.cur_view;
        send_to_replica.channel_map.remove(&replica_last_view);
        send_to_replica.cur_view += 1;
        let ViewQueue {
            sender_chan: send_replica,
            receiver_chan: recv_replica,
            has_received_proposal: _,
        } = HotShot::<SequencingConsensus, TYPES, I>::create_or_obtain_chan_from_write(
            send_to_replica.cur_view,
            send_to_replica,
        )
        .await;

        let mut task_handles = Vec::new();

        if c_api.inner.quorum_exchange.is_leader(cur_view) {
            let da_leader = DALeader {
                id: hotshot.id,
                consensus: hotshot.hotstuff.clone(),
                high_qc: high_qc.clone(),
                cur_view,
                transactions: txns,
                api: c_api.clone(),
                committee_exchange: c_api.inner.committee_exchange.clone(),
                quorum_exchange: c_api.inner.quorum_exchange.clone(),
                vote_collection_chan: recv_da_vote,
                _pd: PhantomData,
            };
            let hotstuff = hotshot.hotstuff.clone();
            let qc = high_qc.clone();
            let api = c_api.clone();
            let leader_handle = async_spawn(async move {
                let Some((da_cert, block, parent)) = da_leader.run_view().await else {
                    return qc;
                };
                let consensus_leader = ConsensusLeader {
                    id: hotshot.id,
                    consensus: hotstuff,
                    high_qc: qc,
                    cert: da_cert,
                    block,
                    parent,
                    cur_view,
                    api: api.clone(),
                    quorum_exchange: api.inner.quorum_exchange.clone(),
                    _pd: PhantomData,
                };
                consensus_leader.run_view().await
            });
            task_handles.push(leader_handle);
        }
        if c_api.inner.quorum_exchange.is_leader(cur_view + 1) {
            let next_leader = ConsensusNextLeader {
                id: hotshot.id,
                consensus: hotshot.hotstuff.clone(),
                cur_view,
                api: c_api.clone(),
                generic_qc: high_qc.clone(),
                vote_collection_chan: recv_commitment_vote_chan,
                quorum_exchange: c_api.inner.quorum_exchange.clone(),
                _pd: PhantomData,
            };
            let next_leader_handle = async_spawn(async move { next_leader.run_view().await });
            task_handles.push(next_leader_handle);
        }
        let da_member = DAMember {
            id: hotshot.id,
            consensus: hotshot.hotstuff.clone(),
            proposal_collection_chan: recv_member,
            cur_view,
            high_qc: high_qc.clone(),
            api: c_api.clone(),
            exchange: c_api.inner.committee_exchange.clone(),
            _pd: PhantomData,
        };
        let member_handle = async_spawn(async move { da_member.run_view().await });
        task_handles.push(member_handle);
        let replica = SequencingReplica {
            id: hotshot.id,
            consensus: hotshot.hotstuff.clone(),
            proposal_collection_chan: recv_replica,
            cur_view,
            high_qc: high_qc.clone(),
            api: c_api.clone(),
            committee_exchange: c_api.inner.committee_exchange.clone(),
            quorum_exchange: c_api.inner.quorum_exchange.clone(),
            _pd: PhantomData,
        };
        let replica_handle = async_spawn(async move { replica.run_view().await });
        task_handles.push(replica_handle);

        let children_finished = futures::future::join_all(task_handles);

        async_spawn({
            let next_view_timeout = hotshot.inner.config.next_view_timeout;
            let hotshot: HotShot<TYPES::ConsensusType, TYPES, I> = hotshot.clone();
            async move {
                async_sleep(Duration::from_millis(next_view_timeout)).await;
                hotshot
                    .timeout_view(cur_view, send_member, Some(send_commitment_vote_chan))
                    .await;
                hotshot
                    .timeout_da_view(cur_view, send_replica, Some(send_da_vote_chan))
                    .await;
            }
        });

        let results = children_finished.await;

        // unwrap is fine since results must have >= 1 item(s)
        #[cfg(feature = "async-std-executor")]
        let high_qc = results
            .into_iter()
            .max_by_key(|qc: &QuorumCertificate<TYPES, SequencingLeaf<TYPES>>| qc.view_number)
            .unwrap();
        #[cfg(feature = "tokio-executor")]
        let high_qc = results
            .into_iter()
            .filter_map(std::result::Result::ok)
            .max_by_key(|qc| qc.view_number)
            .unwrap();

        let mut consensus = hotshot.hotstuff.write().await;
        consensus.high_qc = high_qc;
        c_api.send_view_finished(consensus.cur_view).await;
        Ok(())
    }
}

/// A handle that exposes the interface that hotstuff needs to interact with [`HotShot`]
#[derive(Clone)]
struct HotShotConsensusApi<TYPES: NodeType, I: NodeImplementation<TYPES>> {
    /// Reference to the [`HotShotInner`]
    inner: Arc<HotShotInner<TYPES, I>>,
}

#[async_trait]
impl<TYPES: NodeType, I: NodeImplementation<TYPES>>
    hotshot_consensus::ConsensusApi<TYPES, I::Leaf, I> for HotShotConsensusApi<TYPES, I>
{
    fn total_nodes(&self) -> NonZeroUsize {
        self.inner.config.total_nodes
    }

    fn propose_min_round_time(&self) -> Duration {
        self.inner.config.propose_min_round_time
    }

    fn propose_max_round_time(&self) -> Duration {
        self.inner.config.propose_max_round_time
    }

    fn max_transactions(&self) -> NonZeroUsize {
        self.inner.config.max_transactions
    }

    fn min_transactions(&self) -> usize {
        self.inner.config.min_transactions
    }

    /// Generates and encodes a vote token

    async fn should_start_round(&self, _: TYPES::Time) -> bool {
        false
    }

    async fn send_direct_message<
        PROPOSAL: ProposalType<NodeType = TYPES>,
        VOTE: VoteType<TYPES>,
    >(
        &self,
        recipient: TYPES::SignatureKey,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError> {
        let inner = self.inner.clone();
        debug!(?message, ?recipient, "send_direct_message");
        async_spawn_local(async move {
            inner
                .quorum_exchange
                .network()
                .direct_message(
                    Message {
                        sender: inner.public_key.clone(),
                        kind: message.into(),
                    },
                    recipient,
                )
                .await
        });
        Ok(())
    }

    async fn send_direct_da_message<
        PROPOSAL: ProposalType<NodeType = TYPES>,
        VOTE: VoteType<TYPES>,
    >(
        &self,
        recipient: TYPES::SignatureKey,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError> {
        let inner = self.inner.clone();
        debug!(?message, ?recipient, "send_direct_message");
        async_spawn_local(async move {
            inner
                .committee_exchange
                .network()
                .direct_message(
                    Message {
                        sender: inner.public_key.clone(),
                        kind: message.into(),
                    },
                    recipient,
                )
                .await
        });
        Ok(())
    }

    // TODO remove and use exchange directly.
    async fn send_broadcast_message<
        PROPOSAL: ProposalType<NodeType = TYPES>,
        VOTE: VoteType<TYPES>,
    >(
        &self,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError> {
        debug!(?message, "send_broadcast_message");
        self.inner
            .quorum_exchange
            .network()
            .broadcast_message(
                Message {
                    sender: self.inner.public_key.clone(),
                    kind: message.into(),
                },
                // TODO this is morally wrong!
                &self.inner.quorum_exchange.membership().clone(),
            )
            .await?;
        Ok(())
    }

    async fn send_da_broadcast(
        &self,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError> {
        debug!(?message, "send_da_broadcast_message");
        self.inner
            .committee_exchange
            .network()
            .broadcast_message(
                Message {
                    sender: self.inner.public_key.clone(),
                    kind: message.into(),
                },
                // TODO this is morally wrong!
                &self.inner.committee_exchange.membership().clone(),
            )
            .await?;
        Ok(())
    }

    async fn send_event(&self, event: Event<TYPES, I::Leaf>) {
        debug!(?event, "send_event");
        let mut event_sender = self.inner.event_sender.write().await;
        if let Some(sender) = &*event_sender {
            if let Err(e) = sender.send_async(event).await {
                error!(?e, "Could not send event to event_sender");
                *event_sender = None;
            }
        }
    }

    fn public_key(&self) -> &TYPES::SignatureKey {
        &self.inner.public_key
    }

    fn private_key(&self) -> &<TYPES::SignatureKey as SignatureKey>::PrivateKey {
        &self.inner.private_key
    }

    async fn store_leaf(
        &self,
        old_anchor_view: TYPES::Time,
        leaf: I::Leaf,
    ) -> std::result::Result<(), hotshot_types::traits::storage::StorageError> {
        let view_to_insert = StoredView::from(leaf);
        let storage = &self.inner.storage;
        storage.append_single_view(view_to_insert).await?;
        storage.cleanup_storage_up_to_view(old_anchor_view).await?;
        storage.commit().await?;
        Ok(())
    }
}

/// initializer struct for creating starting block
pub struct HotShotInitializer<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> {
    /// the leaf specified initialization
    inner: LEAF,
}

impl<TYPES: NodeType, LEAF: LeafType<NodeType = TYPES>> HotShotInitializer<TYPES, LEAF> {
    /// initialize from genesis
    /// # Errors
    /// If we are unable to apply the genesis block to the default state
    pub fn from_genesis(genesis_block: TYPES::BlockType) -> Result<Self, HotShotError<TYPES>> {
        let state = TYPES::StateType::default()
            .append(&genesis_block, &TYPES::Time::new(0))
            .map_err(|err| HotShotError::Misc {
                context: err.to_string(),
            })?;
        let time = TYPES::Time::genesis();
        let justify_qc = QuorumCertificate::<TYPES, LEAF>::genesis();

        Ok(Self {
            inner: LEAF::new(time, justify_qc, genesis_block, state),
        })
    }

    /// reload previous state based on most recent leaf
    pub fn from_reload(anchor_leaf: LEAF) -> Self {
        Self { inner: anchor_leaf }
    }
}
