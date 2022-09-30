use std::{sync::Arc, thread::JoinHandle};

use ahash::{AHashMap, AHashSet};
use cid::Cid;
use crossbeam::channel::{Receiver, Sender};
use derivative::Derivative;
use libp2p::PeerId;
use tracing::info;

use crate::client::{
    block_presence_manager::BlockPresenceManager, peer_manager::PeerManager,
    session_manager::SessionManager, session_peer_manager::SessionPeerManager,
};

use super::{
    peer_response_tracker::PeerResponseTracker, sent_want_blocks_tracker::SentWantBlocksTracker,
};

/// Maximum number of changes to accept before blocking
const CHANGES_BUFFER_SIZE: usize = 128;

/// If the session receives this many DONT_HAVEs in a row from a peer,
/// it prunes the peer from the session
const PEER_DONT_HAVE_LIMIT: usize = 16;

/// Indicates whether a peer has a block.
///
/// Note that the order is important, we decide which peer to send a want to
/// based on knowing whether peer has the block. eg we're more likely to send
/// a want to a peer that has the block than a peer that doesnt have the block
/// so BPHave > BPDontHave
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum BlockPresence {
    DontHave = 0,
    Unknown = 1,
    Have = 2,
}

/// Encapsulates a message received by the session.
#[derive(Debug)]
struct Update {
    /// Which peer sent the update
    from: PeerId,
    /// cids of blocks received
    keys: Vec<Cid>,
    /// HAVE message
    haves: Vec<Cid>,
    /// DONT_HAVE message
    dont_haves: Vec<Cid>,
}

/// Indicates a peer's connection state
#[derive(Debug)]
struct PeerAvailability {
    target: PeerId,
    is_available: bool,
}

/// Can be new wants, a new message received by the session, or a change in the
/// connect status of a peer.
#[derive(Debug)]
enum Change {
    /// New wants requested.
    Add(Vec<Cid>),
    /// Wants cancelled.
    Cancel(Vec<Cid>),
    /// New message received by session (blocks / HAVEs / DONT_HAVEs).
    Update(Update),
    /// Peer has connected / disconnected.
    Availability(PeerAvailability),
}

/// Convenience structs for passing around want-blocks and want-haves for a peer.
#[derive(Default, Debug, PartialEq, Eq)]
struct WantSets {
    want_blocks: AHashSet<Cid>,
    want_haves: AHashSet<Cid>,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct AllWants(AHashMap<PeerId, WantSets>);

impl AllWants {
    fn for_peer(&mut self, peer: &PeerId) -> &mut WantSets {
        &mut *self.0.entry(*peer).or_default()
    }
}

// type onSendFn func(to peer.ID, wantBlocks []cid.Cid, wantHaves []cid.Cid)
// type onPeersExhaustedFn func([]cid.Cid)

/// Responsible for sending want-have and want-block to
/// peers. For each want, it sends a single optimistic want-block request to
/// one peer and want-have requests to all other peers in the session.
/// To choose the best peer for the optimistic want-block it maintains a list
/// of how peers have responded to each want (HAVE / DONT_HAVE / Unknown) and
/// consults the peer response tracker (records which peers sent us blocks).
#[derive(Debug, Clone)]
pub struct SessionWantSender {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// The session ID
    session_id: u64,
    /// A channel that collects incoming changes (events)
    changes: Sender<Change>,
    closer: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.closer.send(()).ok();
        self.worker
            .take()
            .expect("missing worker")
            .join()
            .expect("worker paniced");
    }
}

#[derive(Debug, Clone)]
pub struct Signaler {
    id: u64,
    changes: Sender<Change>,
}

impl Signaler {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Called by the `PeerManager` to signal that a peer has connected / disconnected.
    pub fn signal_availability(&self, peer: PeerId, is_available: bool) {
        signal_availability(self.changes.clone(), peer, is_available);
    }
}

fn signal_availability(changes: Sender<Change>, peer: PeerId, is_available: bool) {
    let availability = PeerAvailability {
        target: peer,
        is_available,
    };
    // Add the change in a non-blocking manner to avoid the possibility of a deadlock.
    // TODO: this is bad, fix it
    std::thread::spawn(move || {
        changes
            .send(Change::Availability(availability))
            .expect("sender vanished");
    });
}

impl SessionWantSender {
    pub fn new(
        session_id: u64,
        peer_manager: PeerManager,
        session_peer_manager: SessionPeerManager,
        session_manager: SessionManager,
        block_presence_manager: BlockPresenceManager,
        on_send: Box<dyn Fn(PeerId, Vec<Cid>, Vec<Cid>) + 'static + Send + Sync>,
        on_peers_exhausted: Box<dyn Fn(Vec<Cid>) + 'static + Sync + Send>,
    ) -> Self {
        let (changes_s, changes_r) = crossbeam::channel::bounded(64);
        let (closer_s, closer_r) = crossbeam::channel::bounded(1);

        let signaler = Signaler {
            id: session_id,
            changes: changes_s.clone(),
        };
        let mut loop_state = LoopState::new(
            changes_r.clone(),
            signaler,
            peer_manager,
            session_peer_manager,
            session_manager,
            block_presence_manager,
            on_send,
            on_peers_exhausted,
        );
        let worker = std::thread::spawn(move || {
            // The main loop for processing incoming changes
            loop {
                crossbeam::channel::select! {
                    recv(closer_r) -> _ => {
                        break;
                    }
                    recv(changes_r) -> change => {
                        match change {
                            Ok(change) => { loop_state.on_change(change) },
                            Err(err) => {
                                // sender gone
                                break;
                            }
                        }
                    }
                }
            }
        });

        SessionWantSender {
            inner: Arc::new(Inner {
                session_id,
                changes: changes_s,
                worker: Some(worker),
                closer: closer_s,
            }),
        }
    }

    pub fn id(&self) -> u64 {
        self.inner.session_id
    }

    /// Called when new wants are added to the session
    pub fn add(&self, keys: Vec<Cid>) {
        if keys.is_empty() {
            return;
        }
        self.add_change(Change::Add(keys));
    }

    /// Called when a request is cancelled
    pub fn cancel(&self, keys: Vec<Cid>) {
        if keys.is_empty() {
            return;
        }
        self.add_change(Change::Cancel(keys));
    }

    // Called when the session receives a message with incoming blocks or HAVE / DONT_HAVE.
    pub fn update(&self, from: PeerId, keys: Vec<Cid>, haves: Vec<Cid>, dont_haves: Vec<Cid>) {
        let has_update = !keys.is_empty() || !haves.is_empty() || !dont_haves.is_empty();
        if !has_update {
            return;
        }

        self.add_change(Change::Update(Update {
            from,
            keys,
            haves,
            dont_haves,
        }));
    }

    // Adds a new change to the queue.
    fn add_change(&self, change: Change) {
        self.inner.changes.send(change).ok();
    }

    fn signal_availability(&self, peer: PeerId, is_available: bool) {
        signal_availability(self.inner.changes.clone(), peer, is_available);
    }
}

/// Keeps track of the information for a want
#[derive(Debug)]
struct WantInfo {
    /// Tracks HAVE / DONT_HAVE sent to us for the want by each peer
    block_presence: AHashMap<PeerId, BlockPresence>,
    /// The peer that we've sent a want-block to (cleared when we get a response)
    sent_to: Option<PeerId>,
    /// The "best" peer to send the want to next
    best_peer: Option<PeerId>,
    /// Keeps track of how many hits / misses each peer has sent us for wants in the session.
    peer_response_tracker: PeerResponseTracker,
    /// True if all known peers have sent a DONT_HAVE for this want
    exhausted: bool,
}

impl WantInfo {
    fn new(peer_response_tracker: PeerResponseTracker) -> Self {
        WantInfo {
            block_presence: Default::default(),
            sent_to: None,
            best_peer: None,
            peer_response_tracker,
            exhausted: false,
        }
    }

    /// Called when a HAVE / DONT_HAVE is received for the given want / peer.
    fn update_want_block_presence(
        &mut self,
        block_presence_manager: &BlockPresenceManager,
        cid: &Cid,
        peer: PeerId,
    ) {
        // If the peer sent us a HAVE or DONT_HAVE for the cid, adjust the
        // block presence for the peer / cid combination
        let info = if block_presence_manager.peer_has_block(&peer, cid) {
            BlockPresence::Have
        } else if block_presence_manager.peer_does_not_have_block(&peer, cid) {
            BlockPresence::DontHave
        } else {
            BlockPresence::Unknown
        };
        self.set_peer_block_presence(peer, info);
    }

    /// Sets the block presence for the given peer
    fn set_peer_block_presence(&mut self, peer: PeerId, bp: BlockPresence) {
        self.block_presence.insert(peer, bp);
        self.calculate_best_peer();

        // If a peer informed us that it has a block then make sure the want is no
        // longer flagged as exhausted (exhausted means no peers have the block)
        if bp == BlockPresence::Have {
            self.exhausted = false;
        }
    }

    /// Deletes the given peer from the want info
    fn remove_peer(&mut self, peer: &PeerId) {
        // If we were waiting to hear back from the peer that is being removed,
        // clear the sent_to field so we no longer wait
        if Some(peer) == self.sent_to.as_ref() {
            self.sent_to = None;
        }

        self.block_presence.remove(peer);
        self.calculate_best_peer();
    }

    /// Finds the best peer to send the want to next
    fn calculate_best_peer(&mut self) {
        // Recalculate the best peer
        let mut best_bp = BlockPresence::DontHave;
        let mut best_peer = None;

        // Find the peer with the best block presence, recording how many peers
        // share the block presence
        let mut count_with_best = 0;
        for (peer, bp) in &self.block_presence {
            if bp > &best_bp {
                best_bp = *bp;
                best_peer = Some(*peer);
                count_with_best = 1;
            } else if bp == &best_bp {
                count_with_best += 1;
            }
        }

        self.best_peer = best_peer;

        // If no peer has a block presence better than DONT_HAVE, bail out
        if best_peer.is_none() {
            return;
        }

        // If there was only one peer with the best block presence, we're done
        if count_with_best <= 1 {
            return;
        }

        // There were multiple peers with the best block presence, so choose one of
        // them to be the best
        let mut peers_with_best = Vec::new();
        for (peer, bp) in &self.block_presence {
            if bp == &best_bp {
                peers_with_best.push(*peer);
            }
        }
        self.best_peer = self.peer_response_tracker.choose(&peers_with_best);
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
struct LoopState {
    changes: Receiver<Change>,
    signaler: Signaler,
    /// Information about each want indexed by CID.
    wants: AHashMap<Cid, WantInfo>,
    /// Keeps track of how many consecutive DONT_HAVEs a peer has sent.
    peer_consecutive_dont_haves: AHashMap<PeerId, usize>,
    /// Tracks which peers we have send want-block to.
    sent_want_blocks_tracker: SentWantBlocksTracker,
    /// Tracks the number of blocks each peer sent us
    peer_response_tracker: PeerResponseTracker,
    /// Sends wants to peers
    peer_manager: PeerManager,
    /// Keeps track of peers in the session
    session_peer_manager: SessionPeerManager,
    /// Cancels wants.
    session_manager: SessionManager,
    /// Keeps track of which peer has / doesn't have a block.
    block_presence_manager: BlockPresenceManager,
    /// Called when wants are sent
    #[derivative(Debug = "ignore")]
    on_send: Box<dyn Fn(PeerId, Vec<Cid>, Vec<Cid>) + 'static + Send + Sync>,
    /// Called when all peers explicitly don't have a block
    #[derivative(Debug = "ignore")]
    on_peers_exhausted: Box<dyn Fn(Vec<Cid>) + 'static + Sync + Send>,
}

impl Drop for LoopState {
    fn drop(&mut self) {
        // Unregister the session with the PeerManager
        self.peer_manager.unregister_session(self.signaler.id);
    }
}

impl LoopState {
    fn new(
        changes: Receiver<Change>,
        signaler: Signaler,
        peer_manager: PeerManager,
        session_peer_manager: SessionPeerManager,
        session_manager: SessionManager,
        block_presence_manager: BlockPresenceManager,
        on_send: Box<dyn Fn(PeerId, Vec<Cid>, Vec<Cid>) + 'static + Send + Sync>,
        on_peers_exhausted: Box<dyn Fn(Vec<Cid>) + 'static + Sync + Send>,
    ) -> Self {
        LoopState {
            changes,
            signaler,
            peer_manager,
            wants: Default::default(),
            peer_consecutive_dont_haves: Default::default(),
            sent_want_blocks_tracker: SentWantBlocksTracker::default(),
            peer_response_tracker: PeerResponseTracker::default(),
            session_peer_manager,
            session_manager,
            block_presence_manager,
            on_send,
            on_peers_exhausted,
        }
    }

    fn id(&self) -> u64 {
        self.signaler.id()
    }

    /// Collects all the changes that have occurred since the last invocation of `on_change`.
    fn collect_changes(&self, changes: &mut Vec<Change>) {
        while changes.len() < CHANGES_BUFFER_SIZE {
            if let Ok(change) = self.changes.recv() {
                changes.push(change);
            } else {
                break;
            }
        }
    }

    /// Processes the next set of changes
    fn on_change(&mut self, change: Change) {
        // Several changes may have been recorded since the last time we checked,
        // so pop all outstanding changes from the channel
        let mut changes = vec![change];
        self.collect_changes(&mut changes);

        // Apply each change

        let mut availability = AHashMap::with_capacity(changes.len());
        let mut cancels = Vec::new();
        let mut updates = Vec::new();
        for change in changes {
            match change {
                Change::Add(cids) => {
                    // Initialize info for new wants
                    for cid in cids {
                        self.track_want(cid);
                    }
                }
                Change::Cancel(cids) => {
                    // Remove cancelled wants
                    for cid in cids {
                        self.untrack_want(&cid);
                        cancels.push(cid);
                    }
                }
                Change::Update(update) => {
                    // Consolidate updates and changes to availability
                    // If the update includes blocks or haves, treat it as signaling that
                    // the peer is available
                    if !update.keys.is_empty() || !update.haves.is_empty() {
                        availability.insert(update.from, true);

                        // Register with the PeerManager
                        self.peer_manager
                            .register_session(&update.from, self.signaler.clone());
                    }

                    updates.push(update);
                }
                Change::Availability(PeerAvailability {
                    target,
                    is_available,
                }) => {
                    availability.insert(target, is_available);
                }
            }
        }

        // Update peer availability
        let (newly_available, newly_unavailable) = self.process_availability(&availability);

        // Update wants
        let dont_haves = self.process_updates(updates);

        // Check if there are any wants for which all peers have indicated they don't have the want.
        self.check_for_exhausted_wants(dont_haves, newly_unavailable);

        // If there are any cancels, send them
        if !cancels.is_empty() {
            self.session_manager
                .cancel_session_wants(self.id(), &cancels);
        }

        // If there are some connected peers, send any pending wants
        if self.session_peer_manager.has_peers() {
            self.send_next_wants(newly_available);
        }
    }

    // Updates the want queue with any changes in peer availability
    // It returns the peers that have become
    // - newly available
    // - newly unavailable
    fn process_availability(
        &mut self,
        availability: &AHashMap<PeerId, bool>,
    ) -> (Vec<PeerId>, Vec<PeerId>) {
        let mut newly_available = Vec::new();
        let mut newly_unavailable = Vec::new();
        for (peer, is_now_available) in availability {
            let mut state_change = false;
            if *is_now_available {
                let is_new_peer = self.session_peer_manager.add_peer(peer);
                if is_new_peer {
                    state_change = true;
                    newly_available.push(*peer);
                }
            } else {
                let was_available = self.session_peer_manager.remove_peer(peer);
                if was_available {
                    state_change = true;
                    newly_unavailable.push(*peer);
                }
            }

            // If the state has changed
            if state_change {
                self.update_wants_peer_availability(peer, *is_now_available);
                // Reset the count of consecutive DONT_HAVEs received from the peer.
                self.peer_consecutive_dont_haves.remove(peer);
            }
        }

        (newly_available, newly_unavailable)
    }

    /// Creates a new entry in the map of cid -> want info.
    fn track_want(&mut self, cid: Cid) {
        if self.wants.contains_key(&cid) {
            return;
        }

        // Create the want info
        let mut want_info = WantInfo::new(self.peer_response_tracker.clone());

        // For each available peer, register any information we know about
        // whether the peer has the block
        for peer in self.session_peer_manager.peers() {
            want_info.update_want_block_presence(&self.block_presence_manager, &cid, peer);
        }

        self.wants.insert(cid, want_info);
    }

    // Removes an entry from the map of cid -> want info.
    fn untrack_want(&mut self, cid: &Cid) {
        self.wants.remove(cid);
    }

    /// Processes incoming blocks and HAVE / DONT_HAVEs. It returns all DONT_HAVEs.
    fn process_updates(&mut self, updates: Vec<Update>) -> AHashSet<Cid> {
        // Process received blocks keys
        let mut block_cids = AHashSet::new();
        for update in &updates {
            for cid in &update.keys {
                block_cids.insert(*cid);

                // Remove the want
                if self.remove_want(cid).is_some() {
                    // Inform the peer tracker that this peer was the first to send us the block.
                    self.peer_response_tracker.received_block_from(&update.from);

                    // Protect the connection to this peer so that we can ensure
                    // that the connection doesn't get pruned by the connection manager.
                    self.session_peer_manager.protect_connection(&update.from);
                    self.peer_consecutive_dont_haves.remove(&update.from);
                }
            }
        }

        // Process received DONT_HAVEs
        let mut dont_haves = AHashSet::new();
        let mut prune_peers = AHashSet::new();

        for update in &updates {
            for cid in &update.dont_haves {
                // Track the number of consecutive DONT_HAVEs each peer receives.
                let entry = self
                    .peer_consecutive_dont_haves
                    .entry(update.from)
                    .or_default();
                if *entry == PEER_DONT_HAVE_LIMIT {
                    prune_peers.insert(update.from);
                } else {
                    *entry += 1;
                }

                // If we already received a block for the want, there's no need to update block presence etc.
                if block_cids.contains(cid) {
                    continue;
                }

                dont_haves.insert(*cid);

                // Update the block presence for the peer
                if let Some(wi) = self.wants.get_mut(cid) {
                    wi.update_want_block_presence(&self.block_presence_manager, cid, update.from);
                }

                // Check if the DONT_HAVE is in response to a want-block
                // (could also be in response to want-have)
                if self
                    .sent_want_blocks_tracker
                    .have_sent_want_block_to(&update.from, cid)
                {
                    // If we were waiting for a response from this peer, clear
                    // sentTo so that we can send the want to another peer
                    if let Some(sent_to) = self.get_want_sent_to(cid) {
                        if sent_to == update.from {
                            self.set_want_sent_to(cid, None);
                        }
                    }
                }
            }
        }

        // Process received HAVEs
        for update in &updates {
            for cid in &update.haves {
                // If we haven't already received a block for the want
                if !block_cids.contains(cid) {
                    // Update the block presence for the peer
                    if let Some(wi) = self.wants.get_mut(cid) {
                        wi.update_want_block_presence(
                            &self.block_presence_manager,
                            cid,
                            update.from,
                        );
                    }
                }

                // Clear the consecutive DONT_HAVE count for the peer
                self.peer_consecutive_dont_haves.remove(&update.from);
                prune_peers.remove(&update.from);
            }
        }

        // If any peers have sent us too many consecutive DONT_HAVEs, remove them from the session.
        {
            // Before removing the peer from the session, check if the peer
            // sent us a HAVE for a block that we want
            prune_peers.retain(|peer| {
                for cid in self.wants.keys() {
                    if self.block_presence_manager.peer_has_block(&peer, cid) {
                        return false;
                    }
                }
                true
            });
        }
        if !prune_peers.is_empty() {
            for peer in prune_peers {
                // Peer doesn't have anything we want, so remove it
                info!(
                    "peer {} sent too many dont haves, removing from session {}",
                    peer,
                    self.id()
                );
                self.signaler.signal_availability(peer, false);
            }
        }

        dont_haves
    }

    /// Checks if there are any wants for which all peers have sent a DONT_HAVE. We call these "exhausted" wants.
    fn check_for_exhausted_wants(
        &mut self,
        dont_haves: AHashSet<Cid>,
        newly_unavailable: Vec<PeerId>,
    ) {
        // If there are no new DONT_HAVEs, and no peers became unavailable, then
        // we don't need to check for exhausted wants
        if dont_haves.is_empty() && newly_unavailable.is_empty() {
            return;
        }

        // We need to check each want for which we just received a DONT_HAVE
        let mut wants = dont_haves;

        // If a peer just became unavailable, then we need to check all wants
        // (because it may be the last peer who hadn't sent a DONT_HAVE for a CID)
        if !newly_unavailable.is_empty() {
            // Collect all pending wants
            for cid in self.wants.keys() {
                wants.insert(*cid);
            }

            // If the last available peer in the session has become unavailable
            // then we need to broadcast all pending wants
            if !self.session_peer_manager.has_peers() {
                self.process_exhausted_wants(wants);
                return;
            }
        }

        // If all available peers for a cid sent a DONT_HAVE, signal to the session
        // that we've exhausted available peers
        if !wants.is_empty() {
            let exhausted = self
                .block_presence_manager
                .all_peers_do_not_have_block(&self.session_peer_manager.peers(), wants);
            self.process_exhausted_wants(exhausted);
        }
    }

    /// Filters the list so that only those wants that haven't already been marked as exhausted
    /// are passed to `on_peers_exhausted`.
    fn process_exhausted_wants(&mut self, exhausted: impl IntoIterator<Item = Cid>) {
        let newly_exhausted = self.newly_exhausted(exhausted.into_iter());
        if !newly_exhausted.is_empty() {
            (self.on_peers_exhausted)(newly_exhausted);
        }
    }

    /// Sends wants to peers according to the latest information about which peers have / dont have blocks.
    fn send_next_wants(&mut self, newly_available: Vec<PeerId>) {
        let mut to_send = AllWants::default();

        for (cid, wi) in &mut self.wants {
            // Ensure we send want-haves to any newly available peers
            for peer in &newly_available {
                to_send.for_peer(peer).want_haves.insert(*cid);
            }

            // We already sent a want-block to a peer and haven't yet received a response yet.
            if wi.sent_to.is_some() {
                continue;
            }

            // All the peers have indicated that they don't have the block
            // corresponding to this want, so we must wait to discover more peers
            if let Some(ref best_peer) = wi.best_peer {
                // Record that we are sending a want-block for this want to the peer
                wi.sent_to = Some(*best_peer);

                // Send a want-block to the chosen peer.
                to_send.for_peer(best_peer).want_blocks.insert(*cid);

                // Send a want-have to each other peer.
                for op in self.session_peer_manager.peers() {
                    if &op != best_peer {
                        to_send.for_peer(&op).want_haves.insert(*cid);
                    }
                }
            }
        }

        // Send any wants we've collected
        self.send_wants(to_send);
    }

    /// Sends want-have and want-blocks to the appropriate peers.
    fn send_wants(&mut self, sends: AllWants) {
        // For each peer we're sending a request to
        for (peer, mut snd) in sends.0 {
            // Piggyback some other want-haves onto the request to the peer.
            for cid in self.get_piggyback_want_haves(&peer, &snd.want_blocks) {
                snd.want_haves.insert(cid);
            }

            // Send the wants to the peer.
            // Note that the PeerManager ensures that we don't sent duplicate
            // want-haves / want-blocks to a peer, and that want-blocks take
            // precedence over want-haves.
            let want_blocks: Vec<_> = snd.want_blocks.into_iter().collect();
            let want_haves: Vec<_> = snd.want_haves.into_iter().collect();
            self.peer_manager
                .send_wants(&peer, &want_blocks, &want_haves);
            // Record which peers we send want-block to
            self.sent_want_blocks_tracker
                .add_sent_want_blocks_to(&peer, &want_blocks);

            // Inform the session that we've sent the wants.
            (self.on_send)(peer, want_blocks, want_haves);
        }
    }

    /// Gets the want-haves that should be piggybacked onto a request that we are making to send
    /// want-blocks to a peer.
    fn get_piggyback_want_haves(&self, peer: &PeerId, want_blocks: &AHashSet<Cid>) -> Vec<Cid> {
        let mut res = Vec::new();

        for cid in self.wants.keys() {
            // Don't send want-have if we're already sending a want-block (or have previously).
            if !want_blocks.contains(cid)
                && !self
                    .sent_want_blocks_tracker
                    .have_sent_want_block_to(peer, cid)
            {
                res.push(*cid);
            }
        }
        res
    }

    /// Filters the list of keys for wants that have not already been marked as exhausted
    /// (all peers indicated they don't have the block).
    fn newly_exhausted(&mut self, keys: impl Iterator<Item = Cid>) -> Vec<Cid> {
        keys.filter(|cid| {
            if let Some(wi) = self.wants.get_mut(&cid) {
                if !wi.exhausted {
                    wi.exhausted = true;
                    return true;
                }
            }
            false
        })
        .collect()
    }

    /// Called when the corresponding block is received.
    fn remove_want(&mut self, cid: &Cid) -> Option<WantInfo> {
        self.wants.remove(cid)
    }

    /// Called when the availability changes for a peer. It updates all the wants accordingly.
    fn update_wants_peer_availability(&mut self, peer: &PeerId, is_now_available: bool) {
        for (cid, wi) in &mut self.wants {
            if is_now_available {
                wi.update_want_block_presence(&self.block_presence_manager, cid, *peer);
            } else {
                wi.remove_peer(peer);
            }
        }
    }

    // Which peer was the want sent to.
    fn get_want_sent_to(&self, cid: &Cid) -> Option<PeerId> {
        self.wants.get(cid).and_then(|wi| wi.sent_to)
    }

    // Record which peer the want was sent to
    fn set_want_sent_to(&mut self, cid: &Cid, peer: Option<PeerId>) {
        if let Some(wi) = self.wants.get_mut(cid) {
            wi.sent_to = peer;
        }
    }
}
