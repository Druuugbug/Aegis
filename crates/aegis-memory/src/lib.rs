/// aegis-memory: mempalace patterns for Aegis
///
/// Implements:
/// 1. FileSystem trait + MountableFS (radix-trie style routing)
/// 2. ContextBuilder (layered context loading with token budget)
/// 3. Content-Addressed Storage (SHA-256 keyed blobs)
/// 4. Drawer/Closet storage model (verbatim chunking + secondary index)
/// 5. Memory Taxonomy (Wing/Room/Drawer hierarchy; halls are content-type tags)
/// 6. Tunnel (cross-wing associations)
/// 7. Hybrid Search (vector + BM25 scoring)
/// 8. Write-Ahead Log (WAL)
/// 9. Skill Requirements checking
pub mod cas;
pub mod context;
pub mod entry;
pub mod filesystem;
pub mod hybrid_search;
pub mod sidecar;
pub mod skill;
pub mod taxonomy;
pub mod wal;

pub use cas::{CasEntry, ContentAddressedStorage};
pub use context::{ContextBuilder, ContextSection};
pub use entry::{
    available_disk_bytes, disk_aware_max_entries, MemoryCategory, MemoryEntry, MemoryGraph,
    Reinforcement as MemoryReinforcement, TrustLevel,
};
pub use filesystem::{FileInfo, FileSystem, MemFs, MountableFS, WriteFlag};
pub use hybrid_search::{HybridSearch, SearchResult, SearchScope};
pub use skill::{SkillAvailability, SkillRequirements};
pub use taxonomy::{Closet, Drawer, MemoryTaxonomy, Reinforcement, Room, Tunnel, Wing};
pub use wal::{WalEntry, WriteAheadLog};
