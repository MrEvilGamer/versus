//! VRAM budget policy, unified allocation registry, adaptive VRAM controller,
//! and submission-aware residency planning.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use super::{GuestResource, QueueDeps, ResourceCoherencyManager};

/// Residency priority tier for VRAM budget management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResidencyPriority {
    Low = 0,
    Medium = 1,
    High = 2,
    Critical = 3,
}

/// Unique identifier for a tracked allocation entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AllocationId(pub u64);

/// Unique identifier for the underlying physical backing storage (heap/buffer).
/// Views and aliases share the same `BackingStorageId` so physical size is counted once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackingStorageId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationKind {
    TextureHeap,
    ShadowBuffer,
    RenderTarget,
    Scratch,
    SparseTexture,
    UploadPool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySegment {
    LocalVram,
    HostUpload,
    HostReadback,
    SystemRam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipState {
    CpuAuthoritative,
    GpuAuthoritative,
    Coherent,
    /// Unknown ownership is never treated as safe to evict or skip synchronization.
    Unknown,
}

/// Strict 4-tier reclamation order under DXGI VRAM pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReclaimTier {
    /// 1. Empty heaps and unused speculative allocations.
    EmptyHeap = 0,
    /// 2. Redundant, regenerable host texture copies.
    RegenerableTexture = 1,
    /// 3. Cold CPU-backed shadow buffers.
    ColdShadowBuffer = 2,
    /// 4. Cold pageable heaps with proven restoration safety.
    ColdPageableHeap = 3,
}

/// Unified tracked allocation metadata for the adaptive VRAM controller.
#[derive(Debug, Clone)]
pub struct TrackedAllocation {
    pub id: AllocationId,
    pub backing_id: BackingStorageId,
    pub kind: AllocationKind,
    pub size_bytes: u64,
    pub segment: MemorySegment,
    pub last_access_frame: u64,
    pub access_frequency: u32,
    pub ownership: OwnershipState,
    pub descriptor_refs: u32,
    pub generation: u64,
    pub active_deps: QueueDeps,
    pub restoration_cost: u32,
    pub evictable: bool,
    pub resident: bool,
    pub reclaim_tier: Option<ReclaimTier>,
}

/// Centralized allocation registry that counts each physical backing allocation once.
#[derive(Debug, Default)]
pub struct AllocationRegistry {
    allocations: BTreeMap<AllocationId, TrackedAllocation>,
}

impl AllocationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, alloc: TrackedAllocation) {
        self.allocations.insert(alloc.id, alloc);
    }

    pub fn unregister(&mut self, id: AllocationId) -> Option<TrackedAllocation> {
        self.allocations.remove(&id)
    }

    // Derive backing groups from authoritative entries. In particular, get_mut
    // may change backing IDs or sizes, so a separate cached index can go stale.
    fn backing_groups(&self) -> BTreeMap<BackingStorageId, Vec<&TrackedAllocation>> {
        let mut groups: BTreeMap<BackingStorageId, Vec<&TrackedAllocation>> = BTreeMap::new();
        for allocation in self.allocations.values() {
            groups.entry(allocation.backing_id).or_default().push(allocation);
        }
        groups
    }

    pub fn get(&self, id: AllocationId) -> Option<&TrackedAllocation> {
        self.allocations.get(&id)
    }

    pub fn get_mut(&mut self, id: AllocationId) -> Option<&mut TrackedAllocation> {
        self.allocations.get_mut(&id)
    }

    pub fn touch(&mut self, id: AllocationId, frame: u64, deps: QueueDeps) {
        if let Some(a) = self.allocations.get_mut(&id) {
            a.last_access_frame = frame;
            a.access_frequency = a.access_frequency.saturating_add(1);
            a.active_deps.merge(deps);
        }
    }

    /// Total unique physical bytes across all registered backing allocations.
    pub fn total_unique_backing_bytes(&self) -> u64 {
        self.backing_groups().values()
            .map(|group| group.iter().map(|a| a.size_bytes).max().unwrap_or(0))
            .fold(0u64, u64::saturating_add)
    }

    pub fn unique_backing_count(&self) -> usize {
        self.backing_groups().len()
    }

    /// Plans batched residency for a submission: deduplicates unique backing IDs and
    /// returns non-resident backing allocations to `EnqueueMakeResident`.
    /// When indirect addressing or an unknown ID makes the inventory incomplete,
    /// conservatively includes every tracked non-resident backing.
    pub fn plan_submission_residency(
        &self,
        referenced: &[AllocationId],
        has_bindless_or_indirect: bool,
    ) -> Vec<BackingStorageId> {
        let mut referenced_backings = BTreeSet::new();
        let mut conservative = has_bindless_or_indirect;
        for id in referenced {
            if let Some(a) = self.allocations.get(id) {
                referenced_backings.insert(a.backing_id);
            } else {
                // An incomplete submission inventory cannot prove non-use.
                conservative = true;
            }
        }
        self.backing_groups().into_iter().filter_map(|(backing, group)| {
            ((conservative || referenced_backings.contains(&backing))
                && group.iter().any(|a| !a.resident))
                .then_some(backing)
        }).collect()
    }

    /// Selects fence-safe, ownership-safe eviction candidates in strict 4-tier order
    /// to meet `reclaim_bytes`. Never evicts GPU-authoritative, Unknown, non-evictable,
    /// or GPU-in-flight allocations. Each returned ID represents a whole backing;
    /// callers must revalidate the complete group at eviction time.
    pub fn select_adaptive_evictions(
        &self,
        reclaim_bytes: u64,
        completed_deps: QueueDeps,
    ) -> Vec<AllocationId> {
        if reclaim_bytes == 0 {
            return Vec::new();
        }
        // An eviction is a backing-level operation. Every alias must be safe,
        // including aliases not chosen as the representative allocation ID.
        // Aggregate priority conservatively using the hardest-to-restore alias.
        let mut eligible = Vec::new();
        for group in self.backing_groups().into_values() {
            if !group.iter().all(|a| {
                a.resident
                    && a.evictable
                    && a.segment == MemorySegment::LocalVram
                    && a.descriptor_refs == 0
                    && matches!(a.ownership, OwnershipState::CpuAuthoritative | OwnershipState::Coherent)
                    && a.active_deps.is_satisfied_by(completed_deps)
                    && a.reclaim_tier.is_some()
            }) {
                continue;
            }
            let size = group.iter().map(|a| a.size_bytes).max().unwrap_or(0);
            let tier = group.iter().filter_map(|a| a.reclaim_tier).max().unwrap();
            let cost = group.iter().map(|a| a.restoration_cost).max().unwrap();
            let frequency = group.iter().map(|a| a.access_frequency).max().unwrap();
            let last_access = group.iter().map(|a| a.last_access_frame).max().unwrap();
            // BTreeMap iteration yields a deterministic representative.
            let id = group[0].id;
            eligible.push(((tier, cost, frequency, last_access, Reverse(size), id), size));
        }
        eligible.sort_unstable_by_key(|&(key, _)| key);
        let mut freed = 0u64;
        let mut out = Vec::new();
        for ((_, _, _, _, _, id), size) in eligible {
            if freed >= reclaim_bytes {
                break;
            }
            out.push(id);
            freed = freed.saturating_add(size);
        }
        out
    }

}

/// Adaptive VRAM controller driven by live DXGI budget and usage.
/// Target usage = 85% of current DXGI budget.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveVramController {
    pub budget_bytes: u64,
    pub usage_bytes: u64,
    pub reserved_headroom_bytes: u64,
}

impl AdaptiveVramController {
    pub fn new(budget_bytes: u64, reserved_headroom_bytes: u64) -> Self {
        Self {
            budget_bytes,
            usage_bytes: 0,
            reserved_headroom_bytes,
        }
    }

    pub fn update(&mut self, budget_bytes: u64, usage_bytes: u64) {
        // A failed/invalid DXGI sample must not imply the entire budget vanished.
        if budget_bytes != 0 {
            self.budget_bytes = budget_bytes;
            self.usage_bytes = usage_bytes;
        }
    }

    /// Target usage = 85% of current DXGI budget.
    pub fn target_usage_bytes(&self) -> u64 {
        ((u128::from(self.budget_bytes) * 85) / 100) as u64
    }

    /// Bytes that must be reclaimed to reach target usage plus reserved headroom.
    pub fn reclamation_required_bytes(&self) -> u64 {
        if self.budget_bytes == 0 {
            return 0;
        }
        let effective_target = self
            .target_usage_bytes()
            .saturating_sub(self.reserved_headroom_bytes);
        self.usage_bytes.saturating_sub(effective_target)
    }
}

/// Budget-aware VRAM residency manager with hysteresis and multi-queue fence-safe eviction.
pub struct ResidencyManager {
    pub local_budget_bytes: u64,
    pub local_usage_bytes: u64,
    pub high_watermark_pct: u64,
    pub low_watermark_pct: u64,
}

impl ResidencyManager {
    pub fn new(local_budget_bytes: u64) -> Self {
        Self {
            local_budget_bytes,
            local_usage_bytes: 0,
            high_watermark_pct: 92,
            low_watermark_pct: 82,
        }
    }

    pub fn update_budget(&mut self, budget_bytes: u64, usage_bytes: u64) {
        self.local_budget_bytes = budget_bytes;
        self.local_usage_bytes = usage_bytes;
    }

    /// Selects eviction candidates (never evicting Critical or in-flight GPU resources).
    pub fn select_evictions(&self, mgr: &ResourceCoherencyManager) -> Vec<u64> {
        let high = self.local_budget_bytes * self.high_watermark_pct / 100;
        let low = self.local_budget_bytes * self.low_watermark_pct / 100;
        if self.local_usage_bytes <= high {
            return Vec::new();
        }
        let mut candidates: Vec<&GuestResource> = mgr
            .resources
            .values()
            .filter(|r| {
                r.resident_in_vram
                    && r.priority < ResidencyPriority::Critical
                    && r.active_deps.is_satisfied_by(mgr.completed_deps)
            })
            .collect();
        candidates.sort_by_key(|r| (r.priority, r.last_access_frame));
        let mut freed = 0u64;
        let target_free = self.local_usage_bytes.saturating_sub(low);
        let mut out = Vec::new();
        for r in candidates {
            if freed >= target_free {
                break;
            }
            out.push(r.id);
            freed += r.size;
        }
        out
    }
}
