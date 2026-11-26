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
    backing_sizes: BTreeMap<BackingStorageId, (u64, u32)>,
}

impl AllocationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, alloc: TrackedAllocation) {
        let backing = alloc.backing_id;
        let size = alloc.size_bytes;
        if let Some(prev) = self.allocations.insert(alloc.id, alloc) {
            self.dec_backing(prev.backing_id);
        }
        let entry = self.backing_sizes.entry(backing).or_insert((size, 0));
        entry.0 = entry.0.max(size);
        entry.1 += 1;
    }

    pub fn unregister(&mut self, id: AllocationId) -> Option<TrackedAllocation> {
        let removed = self.allocations.remove(&id)?;
        self.dec_backing(removed.backing_id);
        Some(removed)
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
        self.backing_sizes.values().map(|(sz, _)| *sz).sum()
    }

    pub fn unique_backing_count(&self) -> usize {
        self.backing_sizes.len()
    }

    /// Plans batched residency for a submission: deduplicates unique backing IDs and
    /// returns non-resident backing allocations to `EnqueueMakeResident`.
    /// When `has_bindless_or_indirect` is true, conservatively includes all resident-eligible
    /// non-resident allocations that could be referenced indirectly.
    pub fn plan_submission_residency(
        &self,
        referenced: &[AllocationId],
        has_bindless_or_indirect: bool,
    ) -> Vec<BackingStorageId> {
        let mut need = BTreeSet::new();
        for id in referenced {
            if let Some(a) = self.allocations.get(id)
                && !a.resident
            {
                need.insert(a.backing_id);
            }
        }
        if has_bindless_or_indirect {
            for a in self.allocations.values() {
                if !a.resident && a.descriptor_refs > 0 {
                    need.insert(a.backing_id);
                }
            }
        }
        need.into_iter().collect()
    }

    /// Selects fence-safe, ownership-safe eviction candidates in strict 4-tier order
    /// to meet `reclaim_bytes`. Never evicts GPU-authoritative, Unknown, non-evictable,
    /// or GPU-in-flight allocations.
    pub fn select_adaptive_evictions(
        &self,
        reclaim_bytes: u64,
        completed_deps: QueueDeps,
    ) -> Vec<AllocationId> {
        if reclaim_bytes == 0 {
            return Vec::new();
        }
        let mut eligible: Vec<&TrackedAllocation> = self
            .allocations
            .values()
            .filter(|a| {
                a.resident
                    && a.evictable
                    && a.segment == MemorySegment::LocalVram
                    && matches!(
                        a.ownership,
                        OwnershipState::CpuAuthoritative | OwnershipState::Coherent
                    )
                    && a.active_deps.is_satisfied_by(completed_deps)
                    && a.reclaim_tier.is_some()
            })
            .collect();

        eligible.sort_by_key(|a| {
            (
                a.reclaim_tier.unwrap_or(ReclaimTier::ColdPageableHeap),
                a.restoration_cost,
                a.access_frequency,
                a.last_access_frame,
                Reverse(a.size_bytes),
            )
        });

        let mut seen_backing = BTreeSet::new();
        let mut freed = 0u64;
        let mut out = Vec::new();
        for a in eligible {
            if freed >= reclaim_bytes {
                break;
            }
            if seen_backing.insert(a.backing_id) {
                out.push(a.id);
                freed = freed.saturating_add(a.size_bytes);
            }
        }
        out
    }

    fn dec_backing(&mut self, backing: BackingStorageId) {
        if let Some((_, count)) = self.backing_sizes.get_mut(&backing) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.backing_sizes.remove(&backing);
            }
        }
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
        self.budget_bytes = budget_bytes;
        self.usage_bytes = usage_bytes;
    }

    /// Target usage = 85% of current DXGI budget.
    pub fn target_usage_bytes(&self) -> u64 {
        self.budget_bytes.saturating_mul(85) / 100
    }

    /// Bytes that must be reclaimed to reach target usage plus reserved headroom.
    pub fn reclamation_required_bytes(&self) -> u64 {
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
