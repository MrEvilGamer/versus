//! Guest GPU Virtualization Layer (GGVL) core runtime:
//! - Logical guest address space & sparse 4 KiB page table (separate from 64 KiB D3D12 tiles)
//! - Interval-based `GuestResourceRegistry` with aliasing & lifetime tracking
//! - Event-driven `ResourceCoherencyManager` (authoritative ownership + lazy sync + fence tracking)
//! - Budget-aware `ResidencyManager` with hysteresis & fence-safe eviction
//! - `WaveClass` shader classification (Class A/B/C/D) for NVIDIA Wave32 vs AMD Wave64

use std::collections::{BTreeMap, HashMap};

/// Logical guest tracking page size (4 KiB).
pub const GUEST_PAGE_SIZE: u64 = 4096;
/// Hardware D3D12 reserved-resource tile size (64 KiB).
pub const D3D12_TILE_SIZE: u64 = 65_536;

/// Authoritative ownership state for a tracked guest memory range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipState {
    Uninitialized,
    CpuAuthoritative,
    GpuAuthoritative,
    Coherent,
    CpuToGpuPending { fence: u64 },
    GpuToCpuPending { fence: u64 },
    DerivedConversionPending { fence: u64 },
}

/// Residency priority tier for VRAM budget management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResidencyPriority {
    Low = 0,
    Medium = 1,
    High = 2,
    Critical = 3,
}

/// Wave64 -> Wave32 shader lowering classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaveClass {
    /// Class A: No observable wave-width dependency -> native Wave32.
    WaveIndependent,
    /// Class B: Subgroup ops provably equivalent on Wave32 -> native Wave32.
    Wave32Compatible,
    /// Class C: Compute shader with valid cross-wave LDS lowering.
    SelectiveWave64,
    /// Class D: Graphics/divergent/unsupported -> strict compatibility fallback.
    StrictFallback,
}

/// Action emitted by `ResourceCoherencyManager` on a consumer access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// No hash, compare, upload, or readback required.
    NoOp,
    /// Upload only the dirty guest byte ranges to the host GPU resource.
    UploadRanges(Vec<(u64, u64)>),
    /// Synchronize GPU-to-GPU via D3D12 barrier/fence without CPU involvement.
    GpuBarrierOnly { wait_fence: u64 },
    /// Download only the requested range after waiting on `wait_fence`.
    DownloadRange { start: u64, len: u64, wait_fence: u64 },
    /// Re-derive aliased texture/RT representation on GPU.
    ConvertDerived { wait_fence: u64 },
}

/// Per-resource metadata in the GGVL registry.
#[derive(Debug, Clone)]
pub struct GuestResource {
    pub id: u64,
    pub guest_addr: u64,
    pub size: u64,
    pub is_derived_texture: bool,
    pub ownership: OwnershipState,
    pub cpu_gen: u64,
    pub gpu_gen: u64,
    pub last_gpu_fence: u64,
    pub priority: ResidencyPriority,
    pub resident_in_vram: bool,
    pub last_access_frame: u64,
    /// Dirty 4 KiB page indices relative to `guest_addr`.
    pub dirty_pages: Vec<u32>,
}

/// Sparse 2-level page table mapping 4 KiB guest pages to active resource IDs.
#[derive(Default)]
pub struct GuestMemoryPageTable {
    pages: BTreeMap<u64, Vec<u64>>,
}

impl GuestMemoryPageTable {
    pub fn register_range(&mut self, id: u64, guest_addr: u64, size: u64) {
        if size == 0 {
            return;
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (guest_addr + size - 1) / GUEST_PAGE_SIZE;
        for p in first..=last {
            let entry = self.pages.entry(p).or_default();
            if !entry.contains(&id) {
                entry.push(id);
            }
        }
    }

    pub fn unregister_range(&mut self, id: u64, guest_addr: u64, size: u64) {
        if size == 0 {
            return;
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (guest_addr + size - 1) / GUEST_PAGE_SIZE;
        for p in first..=last {
            if let Some(entry) = self.pages.get_mut(&p) {
                entry.retain(|&x| x != id);
                if entry.is_empty() {
                    self.pages.remove(&p);
                }
            }
        }
    }

    /// Returns all resource IDs overlapping `[guest_addr, guest_addr + size)`.
    pub fn overlapping(&self, guest_addr: u64, size: u64) -> Vec<u64> {
        if size == 0 {
            return Vec::new();
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (guest_addr + size - 1) / GUEST_PAGE_SIZE;
        let mut out = Vec::new();
        for (_, ids) in self.pages.range(first..=last) {
            for &id in ids {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }
}

/// Event-driven coherency & aliasing manager replacing per-frame hash/compare scans.
#[derive(Default)]
pub struct ResourceCoherencyManager {
    next_id: u64,
    pub page_table: GuestMemoryPageTable,
    pub resources: HashMap<u64, GuestResource>,
    pub completed_fence: u64,
    pub skipped_hash_passes: u64,
}

impl ResourceCoherencyManager {
    pub fn register(
        &mut self,
        guest_addr: u64,
        size: u64,
        is_derived_texture: bool,
        priority: ResidencyPriority,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.page_table.register_range(id, guest_addr, size);
        self.resources.insert(
            id,
            GuestResource {
                id,
                guest_addr,
                size,
                is_derived_texture,
                ownership: OwnershipState::Uninitialized,
                cpu_gen: 0,
                gpu_gen: 0,
                last_gpu_fence: 0,
                priority,
                resident_in_vram: true,
                last_access_frame: 0,
                dirty_pages: Vec::new(),
            },
        );
        id
    }

    pub fn unregister(&mut self, id: u64) -> bool {
        if let Some(res) = self.resources.get(&id) {
            if res.last_gpu_fence > self.completed_fence {
                // Defer destruction while GPU work is still in flight.
                return false;
            }
            let (addr, size) = (res.guest_addr, res.size);
            self.page_table.unregister_range(id, addr, size);
            self.resources.remove(&id);
            return true;
        }
        false
    }

    /// Record a CPU write to `[guest_addr, guest_addr + size)` and invalidate all aliases.
    pub fn on_cpu_write(&mut self, guest_addr: u64, size: u64) {
        let affected = self.page_table.overlapping(guest_addr, size);
        for id in affected {
            if let Some(res) = self.resources.get_mut(&id) {
                let start = guest_addr.max(res.guest_addr);
                let end = (guest_addr + size).min(res.guest_addr + res.size);
                if start >= end {
                    continue;
                }
                let first_pg = ((start - res.guest_addr) / GUEST_PAGE_SIZE) as u32;
                let last_pg = ((end - 1 - res.guest_addr) / GUEST_PAGE_SIZE) as u32;
                for pg in first_pg..=last_pg {
                    if !res.dirty_pages.contains(&pg) {
                        res.dirty_pages.push(pg);
                    }
                }
                res.cpu_gen += 1;
                res.ownership = OwnershipState::CpuAuthoritative;
            }
        }
    }

    /// Record a GPU write to `id` associated with completion `fence`, invalidating aliases.
    pub fn on_gpu_write(&mut self, id: u64, fence: u64) {
        let Some((addr, size)) = self.resources.get(&id).map(|r| (r.guest_addr, r.size)) else {
            return;
        };
        let aliases = self.page_table.overlapping(addr, size);
        for other_id in aliases {
            if let Some(res) = self.resources.get_mut(&other_id) {
                res.gpu_gen += 1;
                res.last_gpu_fence = res.last_gpu_fence.max(fence);
                res.dirty_pages.clear();
                res.ownership = if other_id == id {
                    OwnershipState::GpuAuthoritative
                } else if res.is_derived_texture {
                    OwnershipState::DerivedConversionPending { fence }
                } else {
                    OwnershipState::GpuAuthoritative
                };
            }
        }
    }

    /// Plan lazy synchronization before a GPU read of `id`.
    pub fn prepare_gpu_read(&mut self, id: u64, frame: u64) -> SyncAction {
        let Some(res) = self.resources.get_mut(&id) else {
            return SyncAction::NoOp;
        };
        res.last_access_frame = frame;
        match res.ownership {
            OwnershipState::Coherent | OwnershipState::Uninitialized => {
                self.skipped_hash_passes += 1;
                SyncAction::NoOp
            }
            OwnershipState::GpuAuthoritative => {
                self.skipped_hash_passes += 1;
                SyncAction::GpuBarrierOnly {
                    wait_fence: res.last_gpu_fence,
                }
            }
            OwnershipState::DerivedConversionPending { fence } => {
                res.ownership = OwnershipState::Coherent;
                SyncAction::ConvertDerived { wait_fence: fence }
            }
            OwnershipState::CpuAuthoritative => {
                res.dirty_pages.sort_unstable();
                res.dirty_pages.dedup();
                let mut ranges = Vec::new();
                for &pg in &res.dirty_pages {
                    let s = res.guest_addr + u64::from(pg) * GUEST_PAGE_SIZE;
                    let e = (s + GUEST_PAGE_SIZE).min(res.guest_addr + res.size);
                    if let Some((_, last_e)) = ranges.last_mut()
                        && *last_e == s
                    {
                        *last_e = e;
                    } else {
                        ranges.push((s, e));
                    }
                }
                res.dirty_pages.clear();
                res.ownership = OwnershipState::Coherent;
                if ranges.is_empty() {
                    SyncAction::NoOp
                } else {
                    SyncAction::UploadRanges(ranges)
                }
            }
            OwnershipState::CpuToGpuPending { fence }
            | OwnershipState::GpuToCpuPending { fence } => {
                SyncAction::GpuBarrierOnly { wait_fence: fence }
            }
        }
    }

    /// Plan lazy synchronization before a CPU read of `[guest_addr, guest_addr + len)`.
    pub fn prepare_cpu_read(&mut self, id: u64, guest_addr: u64, len: u64) -> SyncAction {
        let Some(res) = self.resources.get_mut(&id) else {
            return SyncAction::NoOp;
        };
        match res.ownership {
            OwnershipState::GpuAuthoritative | OwnershipState::DerivedConversionPending { .. } => {
                let wait_fence = res.last_gpu_fence;
                res.ownership = OwnershipState::Coherent;
                SyncAction::DownloadRange {
                    start: guest_addr,
                    len,
                    wait_fence,
                }
            }
            _ => SyncAction::NoOp,
        }
    }
}

/// Budget-aware VRAM residency manager with hysteresis and fence-safe eviction.
pub struct ResidencyManager {
    pub local_budget_bytes: u64,
    pub local_usage_bytes: u64,
    /// Hysteresis: evict when usage > 92% of budget, stop when < 82% of budget.
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
                    && r.last_gpu_fence <= mgr.completed_fence
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

/// Classifies a console shader for NVIDIA Wave32 lowering.
pub fn classify_shader(
    is_compute: bool,
    uses_wave_ops: bool,
    uses_cross_half_lanes: bool,
    has_divergent_barrier: bool,
) -> WaveClass {
    if !uses_wave_ops {
        WaveClass::WaveIndependent
    } else if !uses_cross_half_lanes {
        WaveClass::Wave32Compatible
    } else if is_compute && !has_divergent_barrier {
        WaveClass::SelectiveWave64
    } else {
        WaveClass::StrictFallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_write_then_gpu_read_batches_dirty_ranges() {
        let mut mgr = ResourceCoherencyManager::default();
        let id = mgr.register(0x1000_0000, 65_536, false, ResidencyPriority::High);
        mgr.on_cpu_write(0x1000_0000, 4096);
        mgr.on_cpu_write(0x1000_1000, 4096);
        let act = mgr.prepare_gpu_read(id, 1);
        assert_eq!(act, SyncAction::UploadRanges(vec![(0x1000_0000, 0x1000_2000)]));
        // Unchanged second read skips all hashes/uploads.
        assert_eq!(mgr.prepare_gpu_read(id, 2), SyncAction::NoOp);
        assert_eq!(mgr.skipped_hash_passes, 1);
    }

    #[test]
    fn gpu_write_then_gpu_read_uses_barrier_no_cpu_readback() {
        let mut mgr = ResourceCoherencyManager::default();
        let id = mgr.register(0x2000_0000, 65_536, false, ResidencyPriority::High);
        mgr.on_gpu_write(id, 42);
        assert_eq!(
            mgr.prepare_gpu_read(id, 5),
            SyncAction::GpuBarrierOnly { wait_fence: 42 }
        );
    }

    #[test]
    fn gpu_write_then_cpu_read_downloads_exact_range() {
        let mut mgr = ResourceCoherencyManager::default();
        let id = mgr.register(0x3000_0000, 65_536, false, ResidencyPriority::High);
        mgr.on_gpu_write(id, 99);
        assert_eq!(
            mgr.prepare_cpu_read(id, 0x3000_1000, 512),
            SyncAction::DownloadRange {
                start: 0x3000_1000,
                len: 512,
                wait_fence: 99
            }
        );
    }

    #[test]
    fn aliased_texture_invalidates_on_buffer_gpu_write() {
        let mut mgr = ResourceCoherencyManager::default();
        let buf = mgr.register(0x4000_0000, 65_536, false, ResidencyPriority::High);
        let tex = mgr.register(0x4000_0000, 65_536, true, ResidencyPriority::High);
        mgr.on_gpu_write(buf, 77);
        assert_eq!(
            mgr.prepare_gpu_read(tex, 10),
            SyncAction::ConvertDerived { wait_fence: 77 }
        );
    }

    #[test]
    fn fence_aware_eviction_never_evicts_inflight_or_critical() {
        let mut mgr = ResourceCoherencyManager::default();
        mgr.completed_fence = 10;
        let crit = mgr.register(0x5000_0000, 1024, false, ResidencyPriority::Critical);
        let inflight = mgr.register(0x5001_0000, 1024, false, ResidencyPriority::Low);
        let cold = mgr.register(0x5002_0000, 1024, false, ResidencyPriority::Low);
        mgr.on_gpu_write(inflight, 20); // fence 20 > completed 10
        assert!(!mgr.unregister(inflight)); // must defer destruction
        let mut res_mgr = ResidencyManager::new(2000);
        res_mgr.update_budget(2000, 1950);
        let evict = res_mgr.select_evictions(&mgr);
        assert_eq!(evict, vec![cold]);
        assert!(!evict.contains(&crit));
        assert!(!evict.contains(&inflight));
    }

    #[test]
    fn wave_classifier_assigns_all_four_classes() {
        assert_eq!(classify_shader(false, false, false, false), WaveClass::WaveIndependent);
        assert_eq!(classify_shader(true, true, false, false), WaveClass::Wave32Compatible);
        assert_eq!(classify_shader(true, true, true, false), WaveClass::SelectiveWave64);
        assert_eq!(classify_shader(false, true, true, false), WaveClass::StrictFallback);
        assert_eq!(classify_shader(true, true, true, true), WaveClass::StrictFallback);
    }
}
