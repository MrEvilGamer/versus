//! Guest GPU Virtualization Layer (GGVL) — Milestone 1.1 Hardened Core.
//!
//! - Per-4KiB-page range-level ownership (separate from 64 KiB D3D12 tile size)
//! - Three-stage transactional synchronization: Plan -> Submit -> Complete/Cancel
//! - Generation-checked transfer tickets (concurrent writes during transfers are never lost)
//! - Queue-aware fence dependencies (`graphics`, `compute`, `copy`)
//! - Checked address arithmetic & explicit `GgvlError` (no silent `NoOp` on invalid IDs)
//! - Exact byte-range intersection for aliasing & derived texture invalidation
//! - Budget-aware `ResidencyManager` with hysteresis & multi-queue fence-safe eviction

// Keep the existing public ggvl API stable; implementations live in focused modules.
mod address_space;
mod coherency;
mod residency;
mod types;
mod wave;

pub use address_space::GuestMemoryPageTable;
pub use coherency::{GuestResource, ResourceCoherencyManager};
pub use residency::{
    AdaptiveVramController, AllocationId, AllocationKind, AllocationRegistry, BackingStorageId,
    MemorySegment, OwnershipState, ReclaimTier, ResidencyManager, ResidencyPriority,
    TrackedAllocation,
};
pub use types::{D3D12_TILE_SIZE, GUEST_PAGE_SIZE, GgvlError, PageMeta, PageState, QueueDeps, SyncPlan, TransferKind, TransferTicket};
pub use wave::{WaveClass, classify_shader};

#[cfg(test)]
mod tests;
