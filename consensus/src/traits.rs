//! Contains the [`ConsensusApi`] trait.

use async_trait::async_trait;

use hotshot_types::certificate::QuorumCertificate;

use hotshot_types::message::ConsensusMessage;

use hotshot_types::traits::node_implementation::{NodeImplementation, NodeType};
use hotshot_types::traits::storage::StorageError;
use hotshot_types::{
    data::{LeafType, ProposalType},
    error::HotShotError,
    event::{Event, EventType},
    traits::{network::NetworkError, signature_key::SignatureKey},
    vote::VoteType,
};

use std::{num::NonZeroUsize, sync::Arc, time::Duration};

// FIXME these should be nonzero u64s
/// The API that [`HotStuff`] needs to talk to the system. This should be implemented in the `hotshot` crate and passed to all functions on `HotStuff`.
///
/// [`HotStuff`]: struct.HotStuff.html
#[async_trait]
pub trait ConsensusApi<
    TYPES: NodeType,
    LEAF: LeafType<NodeType = TYPES>,
    I: NodeImplementation<TYPES>,
>: Send + Sync
{
    /// Total number of nodes in the network. Also known as `n`.
    fn total_nodes(&self) -> NonZeroUsize;

    /// The minimum amount of time a leader has to wait before sending a propose
    fn propose_min_round_time(&self) -> Duration;

    /// The maximum amount of time a leader can wait before sending a propose.ConsensusApi
    /// If this time is reached, the leader has to send a propose without transactions.
    fn propose_max_round_time(&self) -> Duration;

    /// Store a leaf in the storage
    async fn store_leaf(
        &self,
        old_anchor_view: TYPES::Time,
        leaf: LEAF,
    ) -> Result<(), StorageError>;

    /// Retuns the maximum transactions allowed in a block
    fn max_transactions(&self) -> NonZeroUsize;

    /// Returns the minimum transactions that must be in a block
    fn min_transactions(&self) -> usize;

    /// Returns `true` if hotstuff should start the given round. A round can also be started manually by sending `NewView` to the leader.
    ///
    /// In production code this should probably always return `true`.
    async fn should_start_round(&self, view_number: TYPES::Time) -> bool;

    /// Send a direct message to the given recipient
    async fn send_direct_message<PROPOSAL: ProposalType<NodeType = TYPES>, VOTE: VoteType<TYPES>>(
        &self,
        recipient: TYPES::SignatureKey,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError>;

    /// send a direct message using the DA communication channel
    async fn send_direct_da_message<
        PROPOSAL: ProposalType<NodeType = TYPES>,
        VOTE: VoteType<TYPES>,
    >(
        &self,
        recipient: TYPES::SignatureKey,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError>;

    /// Send a broadcast message to the entire network.
    async fn send_broadcast_message<
        PROPOSAL: ProposalType<NodeType = TYPES>,
        VOTE: VoteType<TYPES>,
    >(
        &self,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError>;

    /// Notify the system of an event within `hotshot-consensus`.
    async fn send_event(&self, event: Event<TYPES, LEAF>);

    /// Get a reference to the public key.
    fn public_key(&self) -> &TYPES::SignatureKey;

    /// Get a reference to the private key.
    fn private_key(&self) -> &<TYPES::SignatureKey as SignatureKey>::PrivateKey;

    // Utility functions

    /// notifies client of an error
    async fn send_view_error(&self, view_number: TYPES::Time, error: Arc<HotShotError<TYPES>>) {
        self.send_event(Event {
            view_number,
            event: EventType::Error { error },
        })
        .await;
    }

    /// notifies client of a replica timeout
    async fn send_replica_timeout(&self, view_number: TYPES::Time) {
        self.send_event(Event {
            view_number,
            event: EventType::ReplicaViewTimeout { view_number },
        })
        .await;
    }

    /// notifies client of a next leader timeout
    async fn send_next_leader_timeout(&self, view_number: TYPES::Time) {
        self.send_event(Event {
            view_number,
            event: EventType::NextLeaderViewTimeout { view_number },
        })
        .await;
    }

    /// sends a decide event down the channel
    async fn send_decide(
        &self,
        view_number: TYPES::Time,
        leaf_views: Vec<LEAF>,
        decide_qc: QuorumCertificate<TYPES, LEAF>,
    ) {
        self.send_event(Event {
            view_number,
            event: EventType::Decide {
                leaf_chain: Arc::new(leaf_views),
                qc: Arc::new(decide_qc),
            },
        })
        .await;
    }

    /// Sends a `ViewFinished` event
    async fn send_view_finished(&self, view_number: TYPES::Time) {
        self.send_event(Event {
            view_number,
            event: EventType::ViewFinished { view_number },
        })
        .await;
    }

    /// Send a broadcast to the DA comitee, stub for now
    async fn send_da_broadcast(
        &self,
        message: ConsensusMessage<TYPES, I>,
    ) -> std::result::Result<(), NetworkError>;
}
