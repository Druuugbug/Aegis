//! # Gossip Protocol
//!
//! IBLT-based gossip protocol for decentralized set reconciliation.
use crate::compact::CompactIblt;
use crate::detector::{DetectionResult, SetDifferenceDetector};
use crate::traits::IbltCodec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub type PeerId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipMessage { pub sender: PeerId, pub sequence: u64, pub iblt: Vec<u8>, pub timestamp: u64 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipConfig { pub gossip_interval: Duration, pub fanout: usize, pub iblt_cells: usize, pub max_message_size: usize }
impl Default for GossipConfig { fn default() -> Self { Self { gossip_interval: Duration::from_secs(5), fanout: 3, iblt_cells: 256, max_message_size: 64 * 1024 } } }

#[derive(Debug, Clone)]
struct PeerState { last_seen: Instant, last_sequence: u64, known_healthy: bool }

pub struct GossipManager { config: GossipConfig, local_peer_id: PeerId, peers: HashMap<PeerId, PeerState>, local_iblt: CompactIblt, sequence: u64, detector: SetDifferenceDetector }
impl GossipManager {
    pub fn new(local_peer_id: impl Into<PeerId>, config: GossipConfig) -> Self { let cells = config.iblt_cells; Self { config, local_peer_id: local_peer_id.into(), peers: HashMap::new(), local_iblt: CompactIblt::new(cells), sequence: 0, detector: SetDifferenceDetector::new() } }
    pub fn add_peer(&mut self, peer_id: PeerId) { self.peers.insert(peer_id, PeerState { last_seen: Instant::now(), last_sequence: 0, known_healthy: true }); }
    pub fn insert_local(&mut self, key: &[u8], value: &[u8]) { self.local_iblt.insert(key, value); }
    pub fn build_message(&mut self) -> GossipMessage { self.sequence += 1; GossipMessage { sender: self.local_peer_id.clone(), sequence: self.sequence, iblt: self.local_iblt.encode(), timestamp: now_millis() } }
    pub fn handle_message(&mut self, msg: GossipMessage) -> Option<DetectionResult> {
        if let Some(peer) = self.peers.get_mut(&msg.sender) { if msg.sequence <= peer.last_sequence { return None; } peer.last_seen = Instant::now(); peer.last_sequence = msg.sequence; }
        let remote = CompactIblt::decode_from(&msg.iblt).ok()?;
        Some(self.detector.detect(&self.local_iblt, &remote))
    }
    pub fn select_gossip_peers(&self) -> Vec<PeerId> { self.peers.iter().filter(|(_, s)| s.known_healthy).take(self.config.fanout).map(|(id, _)| id.clone()).collect() }
    pub fn peer_count(&self) -> usize { self.peers.len() }
}
fn now_millis() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_gossip_build_message() { let mut mgr = GossipManager::new("p1", GossipConfig::default()); mgr.insert_local(b"k", b"v"); let msg = mgr.build_message(); assert_eq!(msg.sender, "p1"); assert_eq!(msg.sequence, 1); }
    #[test]
    fn test_gossip_peer_mgmt() { let mut mgr = GossipManager::new("me", GossipConfig::default()); mgr.add_peer("p2".to_string()); assert_eq!(mgr.peer_count(), 1); }
}
