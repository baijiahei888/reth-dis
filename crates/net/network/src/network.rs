use crate::{
    config::NetworkMode, discovery::DiscoveryEvent, manager::NetworkEvent, message::PeerRequest,
    peers::PeersHandle, session::PeerInfo, FetchClient,
};
use async_trait::async_trait;
use parking_lot::Mutex;
use reth_eth_wire::{DisconnectReason, NewBlock, NewPooledTransactionHashes, SharedTransactions};
use reth_interfaces::sync::{NetworkSyncUpdater, SyncState, SyncStateProvider};
use reth_net_common::bandwidth_meter::BandwidthMeter;
use reth_network_api::{
    NetworkError, NetworkInfo, PeerKind, Peers, PeersInfo, Reputation, ReputationChangeKind,
};
use reth_primitives::{Head, NodeRecord, PeerId, TransactionSigned, H256};
use reth_rpc_types::NetworkStatus;
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::{mpsc, mpsc::UnboundedSender, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;

/// A _shareable_ network frontend. Used to interact with the network.
///
/// See also [`NetworkManager`](crate::NetworkManager).
#[derive(Clone, Debug)]
pub struct NetworkHandle {
    /// The Arc'ed delegate that contains the state.
    inner: Arc<NetworkInner>,
}

// === impl NetworkHandle ===

impl NetworkHandle {
    /// Creates a single new instance.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        num_active_peers: Arc<AtomicUsize>,
        listener_address: Arc<Mutex<SocketAddr>>,
        to_manager_tx: UnboundedSender<NetworkHandleMessage>,
        local_peer_id: PeerId,
        peers: PeersHandle,
        network_mode: NetworkMode,
        bandwidth_meter: BandwidthMeter,
        chain_id: Arc<AtomicU64>,
    ) -> Self {
        let inner = NetworkInner {
            num_active_peers,
            to_manager_tx,
            listener_address,
            local_peer_id,
            peers,
            network_mode,
            bandwidth_meter,
            is_syncing: Arc::new(AtomicBool::new(false)),
            chain_id,
        };
        Self { inner: Arc::new(inner) }
    }

    /// Returns the [`PeerId`] used in the network.
    pub fn peer_id(&self) -> &PeerId {
        &self.inner.local_peer_id
    }

    /// Returns the [`PeersHandle`] that can be cloned and shared.
    ///
    /// The [`PeersHandle`] can be used to interact with the network's peer set.
    pub fn peers_handle(&self) -> &PeersHandle {
        &self.inner.peers
    }

    fn manager(&self) -> &UnboundedSender<NetworkHandleMessage> {
        &self.inner.to_manager_tx
    }

    /// Creates a new [`NetworkEvent`] listener channel.
    pub fn event_listener(&self) -> UnboundedReceiverStream<NetworkEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.manager().send(NetworkHandleMessage::EventListener(tx));
        UnboundedReceiverStream::new(rx)
    }

    /// Returns a new [`DiscoveryEvent`] stream.
    ///
    /// This stream yields [`DiscoveryEvent`]s for each peer that is discovered.
    pub fn discovery_listener(&self) -> UnboundedReceiverStream<DiscoveryEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = self.manager().send(NetworkHandleMessage::DiscoveryListener(tx));
        UnboundedReceiverStream::new(rx)
    }

    /// Returns a new [`FetchClient`] that can be cloned and shared.
    ///
    /// The [`FetchClient`] is the entrypoint for sending requests to the network.
    pub async fn fetch_client(&self) -> Result<FetchClient, oneshot::error::RecvError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.manager().send(NetworkHandleMessage::FetchClient(tx));
        rx.await
    }

    /// Returns [`PeerInfo`] for all connected peers
    pub async fn get_peers(&self) -> Result<Vec<PeerInfo>, oneshot::error::RecvError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.manager().send(NetworkHandleMessage::GetPeerInfo(tx));
        rx.await
    }

    /// Returns [`PeerInfo`] for a given peer.
    ///
    /// Returns `None` if there's no active session to the peer.
    pub async fn get_peer_by_id(
        &self,
        peer_id: PeerId,
    ) -> Result<Option<PeerInfo>, oneshot::error::RecvError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.manager().send(NetworkHandleMessage::GetPeerInfoById(peer_id, tx));
        rx.await
    }

    /// Returns the mode of the network, either pow, or pos
    pub fn mode(&self) -> &NetworkMode {
        &self.inner.network_mode
    }

    /// Sends a [`NetworkHandleMessage`] to the manager
    pub(crate) fn send_message(&self, msg: NetworkHandleMessage) {
        let _ = self.inner.to_manager_tx.send(msg);
    }

    /// Update the status of the node.
    pub fn update_status(&self, head: Head) {
        self.send_message(NetworkHandleMessage::StatusUpdate { head });
    }

    /// Announce a block over devp2p
    ///
    /// Caution: in PoS this is a noop, since new block propagation will happen over devp2p
    pub fn announce_block(&self, block: NewBlock, hash: H256) {
        self.send_message(NetworkHandleMessage::AnnounceBlock(block, hash))
    }

    /// Sends a [`PeerRequest`] to the given peer's session.
    pub fn send_request(&self, peer_id: PeerId, request: PeerRequest) {
        self.send_message(NetworkHandleMessage::EthRequest { peer_id, request })
    }

    /// Send transactions hashes to the peer.
    pub fn send_transactions_hashes(&self, peer_id: PeerId, msg: NewPooledTransactionHashes) {
        self.send_message(NetworkHandleMessage::SendPooledTransactionHashes { peer_id, msg })
    }

    /// Send full transactions to the peer
    pub fn send_transactions(&self, peer_id: PeerId, msg: Vec<Arc<TransactionSigned>>) {
        self.send_message(NetworkHandleMessage::SendTransaction {
            peer_id,
            msg: SharedTransactions(msg),
        })
    }

    /// Provides a shareable reference to the [`BandwidthMeter`] stored on the [`NetworkInner`]
    pub fn bandwidth_meter(&self) -> &BandwidthMeter {
        &self.inner.bandwidth_meter
    }

    /// Send message to gracefully shutdown node.
    ///
    /// This will disconnect all active and pending sessions and prevent
    /// new connections to be established.
    pub async fn shutdown(&self) -> Result<(), oneshot::error::RecvError> {
        let (tx, rx) = oneshot::channel();
        self.send_message(NetworkHandleMessage::Shutdown(tx));
        rx.await
    }
}

// === API Implementations ===

impl PeersInfo for NetworkHandle {
    fn num_connected_peers(&self) -> usize {
        self.inner.num_active_peers.load(Ordering::Relaxed)
    }

    fn local_node_record(&self) -> NodeRecord {
        let id = *self.peer_id();
        let socket_addr = *self.inner.listener_address.lock();
        NodeRecord::new(socket_addr, id)
    }
}

#[async_trait]
impl Peers for NetworkHandle {
    /// Sends a message to the [`NetworkManager`](crate::NetworkManager) to add a peer to the known
    /// set, with the given kind.
    fn add_peer_kind(&self, peer: PeerId, kind: PeerKind, addr: SocketAddr) {
        self.send_message(NetworkHandleMessage::AddPeerAddress(peer, kind, addr));
    }

    /// Sends a message to the [`NetworkManager`](crate::NetworkManager) to remove a peer from the
    /// set corresponding to given kind.
    fn remove_peer(&self, peer: PeerId, kind: PeerKind) {
        self.send_message(NetworkHandleMessage::RemovePeer(peer, kind))
    }

    /// Sends a message to the [`NetworkManager`](crate::NetworkManager)  to disconnect an existing
    /// connection to the given peer.
    fn disconnect_peer(&self, peer: PeerId) {
        self.send_message(NetworkHandleMessage::DisconnectPeer(peer, None))
    }

    /// Sends a message to the [`NetworkManager`](crate::NetworkManager)  to disconnect an existing
    /// connection to the given peer using the provided reason
    fn disconnect_peer_with_reason(&self, peer: PeerId, reason: DisconnectReason) {
        self.send_message(NetworkHandleMessage::DisconnectPeer(peer, Some(reason)))
    }

    /// Send a reputation change for the given peer.
    fn reputation_change(&self, peer_id: PeerId, kind: ReputationChangeKind) {
        self.send_message(NetworkHandleMessage::ReputationChange(peer_id, kind));
    }

    async fn reputation_by_id(&self, peer_id: PeerId) -> Result<Option<Reputation>, NetworkError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.manager().send(NetworkHandleMessage::GetReputationById(peer_id, tx));
        Ok(rx.await?)
    }
}

#[async_trait]
impl NetworkInfo for NetworkHandle {
    fn local_addr(&self) -> SocketAddr {
        *self.inner.listener_address.lock()
    }

    async fn network_status(&self) -> Result<NetworkStatus, NetworkError> {
        let (tx, rx) = oneshot::channel();
        let _ = self.manager().send(NetworkHandleMessage::GetStatus(tx));
        rx.await.map_err(Into::into)
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id.load(Ordering::Relaxed)
    }

    fn is_syncing(&self) -> bool {
        SyncStateProvider::is_syncing(self)
    }
}

impl SyncStateProvider for NetworkHandle {
    fn is_syncing(&self) -> bool {
        self.inner.is_syncing.load(Ordering::Relaxed)
    }
}

impl NetworkSyncUpdater for NetworkHandle {
    fn update_sync_state(&self, state: SyncState) {
        let is_syncing = state.is_syncing();
        self.inner.is_syncing.store(is_syncing, Ordering::Relaxed)
    }

    /// Update the status of the node.
    fn update_status(&self, head: Head) {
        self.send_message(NetworkHandleMessage::StatusUpdate { head });
    }
}

#[derive(Debug)]
struct NetworkInner {
    /// Number of active peer sessions the node's currently handling.
    num_active_peers: Arc<AtomicUsize>,
    /// Sender half of the message channel to the [`crate::NetworkManager`].
    to_manager_tx: UnboundedSender<NetworkHandleMessage>,
    /// The local address that accepts incoming connections.
    listener_address: Arc<Mutex<SocketAddr>>,
    /// The identifier used by this node.
    local_peer_id: PeerId,
    /// Access to the all the nodes.
    peers: PeersHandle,
    /// The mode of the network
    network_mode: NetworkMode,
    /// Used to measure inbound & outbound bandwidth across network streams (currently unused)
    bandwidth_meter: BandwidthMeter,
    /// Represents if the network is currently syncing.
    is_syncing: Arc<AtomicBool>,
    /// The chain id
    chain_id: Arc<AtomicU64>,
}

/// Internal messages that can be passed to the  [`NetworkManager`](crate::NetworkManager).
#[allow(missing_docs)]
pub(crate) enum NetworkHandleMessage {
    /// Adds an address for a peer.
    AddPeerAddress(PeerId, PeerKind, SocketAddr),
    /// Removes a peer from the peerset corresponding to the given kind.
    RemovePeer(PeerId, PeerKind),
    /// Disconnect a connection to a peer if it exists.
    DisconnectPeer(PeerId, Option<DisconnectReason>),
    /// Add a new listener for [`NetworkEvent`].
    EventListener(UnboundedSender<NetworkEvent>),
    /// Broadcast event to announce a new block to all nodes.
    AnnounceBlock(NewBlock, H256),
    /// Sends the list of transactions to the given peer.
    SendTransaction { peer_id: PeerId, msg: SharedTransactions },
    /// Sends the list of transactions hashes to the given peer.
    SendPooledTransactionHashes { peer_id: PeerId, msg: NewPooledTransactionHashes },
    /// Send an `eth` protocol request to the peer.
    EthRequest {
        /// The peer to send the request to.
        peer_id: PeerId,
        /// The request to send to the peer's sessions.
        request: PeerRequest,
    },
    /// Apply a reputation change to the given peer.
    ReputationChange(PeerId, ReputationChangeKind),
    /// Returns the client that can be used to interact with the network.
    FetchClient(oneshot::Sender<FetchClient>),
    /// Apply a status update.
    StatusUpdate { head: Head },
    /// Get the current status
    GetStatus(oneshot::Sender<NetworkStatus>),
    /// Get PeerInfo from all the peers
    GetPeerInfo(oneshot::Sender<Vec<PeerInfo>>),
    /// Get PeerInfo for a specific peer
    GetPeerInfoById(PeerId, oneshot::Sender<Option<PeerInfo>>),
    /// Get the reputation for a specific peer
    GetReputationById(PeerId, oneshot::Sender<Option<Reputation>>),
    /// Gracefully shutdown network
    Shutdown(oneshot::Sender<()>),
    /// Add a new listener for `DiscoveryEvent`.
    DiscoveryListener(UnboundedSender<DiscoveryEvent>),
}
