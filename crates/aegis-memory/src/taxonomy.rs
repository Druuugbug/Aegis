use std::collections::HashMap;

/// Wing/Room/Drawer memory taxonomy (mempalace hierarchy).
/// `halls` are content-type tags on a wing; `closets` are a per-room
/// secondary index; `tunnels` are cross-wing associations.
/// Re-export from entry module for use in Drawer.
pub use crate::entry::Reinforcement;

/// A verbatim chunk of content stored in the palace.
/// Enhanced with confidence scoring, graph links, and supersession tracking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Drawer {
    pub id: String,      // drawer_{wing}_{room}_{hash}
    pub content: String, // verbatim chunk (800 chars, 100 overlap)
    pub wing: String,    // top-level domain
    pub room: String,    // topic
    pub source_file: String,
    pub chunk_index: u32,
    pub filed_at: chrono::DateTime<chrono::Utc>,
    // ── v2 additions: confidence + graph ──
    /// Base confidence (0.0 - 1.0). Defaults to 0.7.
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// Number of times this drawer was accessed/retrieved.
    #[serde(default)]
    pub access_count: u32,
    /// Last time this drawer was accessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Linked drawer IDs (graph edges).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub linked_ids: Vec<String>,
    /// If superseded by a newer drawer, this points to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Reinforcement history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reinforcements: Vec<Reinforcement>,
    /// Whether this drawer is still active (not superseded).
    #[serde(default = "default_active")]
    pub active: bool,
}

fn default_confidence() -> f32 {
    0.7
}
fn default_active() -> bool {
    true
}

impl Drawer {
    /// Create a new drawer with default v2 fields.
    pub fn new(
        id: String,
        content: String,
        wing: String,
        room: String,
        source_file: String,
        chunk_index: u32,
    ) -> Self {
        Self {
            id,
            content,
            wing,
            room,
            source_file,
            chunk_index,
            filed_at: chrono::Utc::now(),
            confidence: 0.7,
            access_count: 0,
            accessed_at: None,
            linked_ids: Vec::new(),
            superseded_by: None,
            reinforcements: Vec::new(),
            active: true,
        }
    }

    /// Compute effective confidence based on access history, age, and reinforcements.
    ///
    /// Formula:
    ///   effective = base_confidence
    ///             + access_boost (ln(access_count) * 0.05)
    ///             - age_decay (min(age_days/365, 0.3))
    ///             + reinforcement_adjustment
    ///   clamped to [0.0, 1.0]
    pub fn effective_confidence(&self) -> f32 {
        let base = self.confidence;

        // Access boost: logarithmic, diminishing returns
        let access_boost = if self.access_count > 1 {
            (self.access_count as f32).ln() * 0.05
        } else {
            0.0
        };

        // Age decay: linear up to 30% over a year
        let age_days = self
            .accessed_at
            .or(Some(self.filed_at))
            .map(|t| {
                chrono::Utc::now()
                    .signed_duration_since(t)
                    .num_days()
                    .max(0) as f32
            })
            .unwrap_or(0.0);
        let age_decay = (age_days / 365.0).min(0.3);

        // Reinforcement adjustment: average of recent scores (last 10)
        let reinforcement_adj = if self.reinforcements.is_empty() {
            0.0
        } else {
            let recent: Vec<f32> = self
                .reinforcements
                .iter()
                .rev()
                .take(10)
                .map(|r| r.score)
                .collect();
            let avg: f32 = recent.iter().sum::<f32>() / recent.len() as f32;
            (avg * 0.2).clamp(-0.15, 0.15)
        };

        (base + access_boost - age_decay + reinforcement_adj).clamp(0.0, 1.0)
    }

    /// Record an access event.
    pub fn touch(&mut self) {
        self.access_count += 1;
        self.accessed_at = Some(chrono::Utc::now());
    }

    /// Record a reinforcement event (positive or negative).
    pub fn reinforce(&mut self, score: f32, context: impl Into<String>) {
        self.reinforcements.push(Reinforcement {
            timestamp: chrono::Utc::now(),
            score: score.clamp(-1.0, 1.0),
            context: context.into(),
            related_to: None,
        });
    }

    /// Decrement base confidence.
    pub fn decay_confidence(&mut self, amount: f32) {
        self.confidence = (self.confidence - amount).max(0.0);
    }

    /// Mark this drawer as superseded by another.
    pub fn supersede(&mut self, new_id: &str) {
        self.superseded_by = Some(new_id.to_string());
        self.active = false;
    }

    /// Add a bidirectional link to another drawer.
    pub fn link_to(&mut self, other_id: &str) {
        if !self.linked_ids.iter().any(|id| id == other_id) {
            self.linked_ids.push(other_id.to_string());
        }
    }
}

/// Closet: compact secondary index — topic → drawer pointers
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Closet {
    pub topic: String,
    pub entities: Vec<String>,
    pub drawer_ids: Vec<String>,
}

/// Explicit cross-wing association
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Tunnel {
    pub source_wing: String,
    pub source_room: String,
    pub target_wing: String,
    pub target_room: String,
    pub relation: String, // "implements", "references", "contradicts"
}

impl Tunnel {
    /// Return the source scope as `wing/room`.
    pub fn source_scope(&self) -> String {
        format!("{}/{}", self.source_wing, self.source_room)
    }

    /// Return the target scope as `wing/room`.
    pub fn target_scope(&self) -> String {
        format!("{}/{}", self.target_wing, self.target_room)
    }
}

#[derive(Debug, Default)]
pub struct Room {
    pub drawers: Vec<Drawer>,
    pub closets: Vec<Closet>,
}

#[derive(Debug, Default)]
pub struct Wing {
    pub rooms: HashMap<String, Room>,
    pub halls: Vec<String>, // content type tags
}

/// Full memory palace
#[derive(Debug, Default)]
pub struct MemoryTaxonomy {
    pub wings: HashMap<String, Wing>,
    pub tunnels: Vec<Tunnel>,
}

impl MemoryTaxonomy {
    /// Create an empty memory taxonomy.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store verbatim text: chunk into 800-char pieces with 100-char overlap.
    pub fn ingest(&mut self, wing: &str, room: &str, source_file: &str, text: &str) {
        const CHUNK_SIZE: usize = 800;
        const OVERLAP: usize = 100;

        let wing_entry = self.wings.entry(wing.to_string()).or_default();
        let room_entry = wing_entry.rooms.entry(room.to_string()).or_default();

        let chars: Vec<char> = text.chars().collect();
        let mut start = 0usize;
        let mut chunk_index = 0u32;

        while start < chars.len() {
            let end = (start + CHUNK_SIZE).min(chars.len());
            let chunk: String = chars[start..end].iter().collect();
            let hash = crate::cas::ContentAddressedStorage::hash(&chunk);
            let id = format!("drawer_{}_{}_{}", wing, room, &hash[..8]);

            room_entry.drawers.push(Drawer::new(
                id,
                chunk,
                wing.to_string(),
                room.to_string(),
                source_file.to_string(),
                chunk_index,
            ));

            if end == chars.len() {
                break;
            }
            start = end.saturating_sub(OVERLAP);
            chunk_index += 1;
        }
    }

    /// Add a closet (secondary index entry).
    pub fn add_closet(&mut self, wing: &str, room: &str, closet: Closet) {
        let wing_entry = self.wings.entry(wing.to_string()).or_default();
        let room_entry = wing_entry.rooms.entry(room.to_string()).or_default();
        room_entry.closets.push(closet);
    }

    /// Add a tunnel (cross-wing relation).
    pub fn add_tunnel(&mut self, tunnel: Tunnel) {
        self.tunnels.push(tunnel);
    }

    /// Get all drawers in a scope (wing/room).
    pub fn drawers_in_scope(&self, wing: &str, room: &str) -> Vec<&Drawer> {
        self.wings
            .get(wing)
            .and_then(|w| w.rooms.get(room))
            .map(|r| r.drawers.iter().collect())
            .unwrap_or_default()
    }

    /// Get tunnels originating from a scope.
    pub fn tunnels_from(&self, wing: &str, room: &str) -> Vec<&Tunnel> {
        self.tunnels
            .iter()
            .filter(|t| t.source_wing == wing && t.source_room == room)
            .collect()
    }

    /// Find a drawer by ID across all wings/rooms.
    pub fn find_drawer(&self, id: &str) -> Option<&Drawer> {
        for wing in self.wings.values() {
            for room in wing.rooms.values() {
                if let Some(d) = room.drawers.iter().find(|d| d.id == id) {
                    return Some(d);
                }
            }
        }
        None
    }

    /// Find a mutable drawer by ID across all wings/rooms.
    pub fn find_drawer_mut(&mut self, id: &str) -> Option<&mut Drawer> {
        for wing in self.wings.values_mut() {
            for room in wing.rooms.values_mut() {
                if let Some(d) = room.drawers.iter_mut().find(|d| d.id == id) {
                    return Some(d);
                }
            }
        }
        None
    }

    /// Get all active (non-superseded) drawer IDs linked to a given drawer.
    pub fn linked_active_ids(&self, drawer_id: &str) -> Vec<String> {
        self.find_drawer(drawer_id)
            .map(|d| {
                d.linked_ids
                    .iter()
                    .filter(|lid| {
                        self.find_drawer(lid)
                            .map(|linked| linked.active)
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Count total drawers, active drawers, and superseded drawers.
    pub fn stats(&self) -> (usize, usize, usize) {
        let mut total = 0;
        let mut active = 0;
        let mut superseded = 0;
        for wing in self.wings.values() {
            for room in wing.rooms.values() {
                for d in &room.drawers {
                    total += 1;
                    if d.active {
                        active += 1;
                    } else {
                        superseded += 1;
                    }
                }
            }
        }
        (total, active, superseded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawer_new_defaults() {
        let d = Drawer::new(
            "d1".into(),
            "hello".into(),
            "dev".into(),
            "rust".into(),
            "main.rs".into(),
            0,
        );
        assert_eq!(d.id, "d1");
        assert_eq!(d.content, "hello");
        assert_eq!(d.confidence, 0.7);
        assert!(d.active);
        assert_eq!(d.access_count, 0);
        assert!(d.linked_ids.is_empty());
        assert!(d.superseded_by.is_none());
        assert!(d.reinforcements.is_empty());
    }

    #[test]
    fn drawer_effective_confidence_fresh() {
        let d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        // Fresh drawer: ~0.7 (no age decay yet, no access boost)
        let eff = d.effective_confidence();
        assert!((0.65..=0.75).contains(&eff), "expected ~0.7, got {}", eff);
    }

    #[test]
    fn drawer_effective_confidence_with_access() {
        let mut d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        d.access_count = 10;
        d.accessed_at = Some(chrono::Utc::now());
        let eff = d.effective_confidence();
        assert!(
            eff > 0.7,
            "access boost should raise confidence, got {}",
            eff
        );
    }

    #[test]
    fn drawer_touch() {
        let mut d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        d.touch();
        assert_eq!(d.access_count, 1);
        assert!(d.accessed_at.is_some());
    }

    #[test]
    fn drawer_reinforce() {
        let mut d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        d.reinforce(0.5, "positive feedback");
        assert_eq!(d.reinforcements.len(), 1);
        assert_eq!(d.reinforcements[0].score, 0.5);
        // Score clamped
        d.reinforce(2.0, "clamped");
        assert_eq!(d.reinforcements[1].score, 1.0);
    }

    #[test]
    fn drawer_decay_confidence() {
        let mut d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        d.decay_confidence(0.3);
        assert!((d.confidence - 0.4).abs() < 0.01);
        d.decay_confidence(1.0);
        assert_eq!(d.confidence, 0.0); // floored at 0
    }

    #[test]
    fn drawer_supersede() {
        let mut d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        d.supersede("d2");
        assert_eq!(d.superseded_by, Some("d2".to_string()));
        assert!(!d.active);
    }

    #[test]
    fn drawer_link_to_no_duplicates() {
        let mut d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        d.link_to("d2");
        d.link_to("d2"); // duplicate
        d.link_to("d3");
        assert_eq!(d.linked_ids.len(), 2);
    }

    #[test]
    fn drawer_effective_confidence_with_reinforcements() {
        let mut d = Drawer::new(
            "d1".into(),
            "c".into(),
            "w".into(),
            "r".into(),
            "f".into(),
            0,
        );
        d.reinforce(0.5, "good");
        d.reinforce(0.5, "good");
        d.reinforce(0.5, "good");
        let eff = d.effective_confidence();
        assert!(
            eff > 0.7,
            "positive reinforcement should boost, got {}",
            eff
        );
    }

    #[test]
    fn tunnel_scopes() {
        let t = Tunnel {
            source_wing: "dev".into(),
            source_room: "rust".into(),
            target_wing: "ops".into(),
            target_room: "deploy".into(),
            relation: "implements".into(),
        };
        assert_eq!(t.source_scope(), "dev/rust");
        assert_eq!(t.target_scope(), "ops/deploy");
    }

    #[test]
    fn memory_taxonomy_ingest_short_text() {
        let mut tax = MemoryTaxonomy::new();
        tax.ingest("dev", "rust", "main.rs", "Hello world");
        assert_eq!(tax.wings.len(), 1);
        assert_eq!(tax.drawers_in_scope("dev", "rust").len(), 1);
    }

    #[test]
    fn memory_taxonomy_ingest_long_text_chunks() {
        let mut tax = MemoryTaxonomy::new();
        // Use varied content so chunks have different hashes
        let long_text: String = (0..2000).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        tax.ingest("dev", "rust", "big.rs", &long_text);
        let drawers = tax.drawers_in_scope("dev", "rust");
        assert!(
            drawers.len() >= 2,
            "expected multiple chunks, got {}",
            drawers.len()
        );
        // All drawers should have unique IDs
        let mut ids: Vec<&str> = drawers.iter().map(|d| d.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), drawers.len());
    }

    #[test]
    fn memory_taxonomy_add_tunnel() {
        let mut tax = MemoryTaxonomy::new();
        tax.add_tunnel(Tunnel {
            source_wing: "a".into(),
            source_room: "b".into(),
            target_wing: "c".into(),
            target_room: "d".into(),
            relation: "references".into(),
        });
        assert_eq!(tax.tunnels.len(), 1);
        assert_eq!(tax.tunnels_from("a", "b").len(), 1);
        assert_eq!(tax.tunnels_from("c", "d").len(), 0); // wrong direction
    }

    #[test]
    fn memory_taxonomy_find_drawer() {
        let mut tax = MemoryTaxonomy::new();
        tax.ingest("dev", "rust", "main.rs", "test content");
        let drawer = &tax.drawers_in_scope("dev", "rust")[0];
        let id = drawer.id.clone();
        assert!(tax.find_drawer(&id).is_some());
        assert!(tax.find_drawer("nonexistent").is_none());
    }

    #[test]
    fn memory_taxonomy_find_drawer_mut_and_supersede() {
        let mut tax = MemoryTaxonomy::new();
        tax.ingest("dev", "rust", "main.rs", "content");
        let id = tax.drawers_in_scope("dev", "rust")[0].id.clone();
        tax.find_drawer_mut(&id).unwrap().supersede("new_id");
        assert!(!tax.find_drawer(&id).unwrap().active);
        let (total, active, superseded) = tax.stats();
        assert_eq!(total, 1);
        assert_eq!(active, 0);
        assert_eq!(superseded, 1);
    }

    #[test]
    fn memory_taxonomy_linked_active_ids() {
        let mut tax = MemoryTaxonomy::new();
        tax.ingest("dev", "rust", "a.rs", "content A");
        tax.ingest("dev", "rust", "b.rs", "content B");
        let id_a = tax.drawers_in_scope("dev", "rust")[0].id.clone();
        let id_b = tax.drawers_in_scope("dev", "rust")[1].id.clone();

        // Link A -> B
        tax.find_drawer_mut(&id_a).unwrap().link_to(&id_b);
        let linked = tax.linked_active_ids(&id_a);
        assert_eq!(linked, vec![id_b.clone()]);

        // Supersede B, then linked should be empty
        tax.find_drawer_mut(&id_b).unwrap().supersede("replacement");
        assert!(tax.linked_active_ids(&id_a).is_empty());
    }

    #[test]
    fn memory_taxonomy_add_closet() {
        let mut tax = MemoryTaxonomy::new();
        tax.add_closet(
            "dev",
            "rust",
            Closet {
                topic: "ownership".into(),
                entities: vec!["borrow checker".into()],
                drawer_ids: vec!["d1".into()],
            },
        );
        let wing = tax.wings.get("dev").unwrap();
        let room = wing.rooms.get("rust").unwrap();
        assert_eq!(room.closets.len(), 1);
        assert_eq!(room.closets[0].topic, "ownership");
    }

    #[test]
    fn memory_taxonomy_stats_empty() {
        let tax = MemoryTaxonomy::new();
        assert_eq!(tax.stats(), (0, 0, 0));
    }

    #[test]
    fn memory_taxonomy_serialization_roundtrip() {
        let mut tax = MemoryTaxonomy::new();
        tax.ingest("dev", "rust", "main.rs", "hello world");
        // Verify the drawers themselves serialize roundtrip
        let drawer = &tax.drawers_in_scope("dev", "rust")[0];
        let json = serde_json::to_string(drawer).unwrap();
        let deserialized: Drawer = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.content, drawer.content);
        assert_eq!(deserialized.wing, drawer.wing);
    }
}
