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

pub use cas::{ContentAddressedStorage, CasEntry};
pub use entry::{MemoryCategory, TrustLevel, Reinforcement as MemoryReinforcement, MemoryEntry, MemoryGraph, available_disk_bytes, disk_aware_max_entries};
pub use context::{ContextBuilder, ContextSection};
pub use filesystem::{FileSystem, FileInfo, WriteFlag, MountableFS, MemFs};
pub use hybrid_search::{HybridSearch, SearchResult, SearchScope};
pub use skill::{SkillRequirements, SkillAvailability};
pub use taxonomy::{MemoryTaxonomy, Wing, Room, Drawer, Closet, Tunnel, Reinforcement};
pub use wal::{WriteAheadLog, WalEntry};
