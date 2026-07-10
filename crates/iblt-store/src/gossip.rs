//! # IBLT Store - Gossip Layer
//!
//! Store-aware gossip protocol for tiered storage with IBLT summaries.
use iblt::compact::CompactIblt;
use iblt::detector::{DetectionResult, SetDifferenceDetector};
use iblt::traits::IbltCodec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StorageTier { Hot, Warm, Cold }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreGossipConfig { pub tier_intervals: HashMap<StorageTier, Duration>, pub tier_cells: HashMap<StorageTier, usize>, pub fanout: usize }
impl Default for StoreGossipConfig {
    fn default() -> Self {
        let mut ti = HashMap::new(); ti.insert(StorageTier::Hot, Duration::from_secs(2)); ti.insert(StorageTier::Warm, Duration::from_secs(10)); ti.insert(StorageTier::Cold, Duration::from_secs(60));
        let mut tc = HashMap::new(); tc.insert(StorageTier::Hot, 512); tc.insert(StorageTier::Warm, 256); tc.insert(StorageTier::Cold, 128);
        Self { tier_intervals: ti, tier_cells: tc, fanout: 3 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorePeer { pub id: String, pub tier: StorageTier, pub reliability: f64 }

pub struct StoreGossipManager { config: StoreGossipConfig, peers: Vec<StorePeer>, tier_iblts: HashMap<StorageTier, CompactIblt>, detector: SetDifferenceDetector }
impl StoreGossipManager {
    pub fn new(config: StoreGossipConfig) -> Self { let mut tib = HashMap::new(); for (&t, &c) in &config.tier_cells { tib.insert(t, CompactIblt::new(c)); } Self { config, peers: Vec::new(), tier_iblts: tib, detector: SetDifferenceDetector::new() } }
    pub fn add_peer(&mut self, peer: StorePeer) { self.peers.push(peer); }
    pub fn insert(&mut self, tier: StorageTier, key: &[u8], value: &[u8]) { if let Some(iblt) = self.tier_iblts.get_mut(&tier) { iblt.insert(key, value); } }
    pub fn build_summary(&self, tier: StorageTier) -> Option<Vec<u8>> { self.tier_iblts.get(&tier).map(|iblt| iblt.encode()) }
    pub fn handle_summary(&self, tier: StorageTier, remote_data: &[u8]) -> anyhow::Result<DetectionResult> { let local = self.tier_iblts.get(&tier).ok_or_else(|| anyhow::anyhow!("unknown tier"))?; let remote = CompactIblt::decode_from(remote_data)?; Ok(self.detector.detect(local, &remote)) }
    pub fn peers_for_tier(&self, tier: StorageTier) -> Vec<&StorePeer> { let mut p: Vec<&StorePeer> = self.peers.iter().filter(|p| p.tier == tier).collect(); p.sort_by(|a, b| b.reliability.partial_cmp(&a.reliability).unwrap_or(std::cmp::Ordering::Equal)); p.truncate(self.config.fanout); p }
    pub fn peer_count(&self) -> usize { self.peers.len() }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_store_gossip() { let mut mgr = StoreGossipManager::new(StoreGossipConfig::default()); mgr.insert(StorageTier::Hot, b"k", b"v"); let summary = mgr.build_summary(StorageTier::Hot); assert!(summary.is_some()); }
}
