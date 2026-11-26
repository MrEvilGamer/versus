//! Guest GPU Virtualization Layer (GGVL) — Milestone 1.1 Hardened Core.
//!
//! - Per-4KiB-page range-level ownership (separate from 64 KiB D3D12 tile size)
//! - Three-stage transactional synchronization: Plan -> Submit -> Complete/Cancel
//! - Generation-checked transfer tickets (concurrent writes during transfers are never lost)
//! - Queue-aware fence dependencies (`graphics`, `compute`, `copy`)
//! - Checked address arithmetic & explicit `GgvlError` (no silent `NoOp` on invalid IDs)
//! - Exact byte-range intersection for aliasing & derived texture invalidation
//! - Budget-aware `ResidencyManager` with hysteresis & multi-queue fence-safe eviction

use std::collections::{BTreeMap, HashMap};

/// Logical guest tracking page size (4 KiB).
pub const GUEST_PAGE_SIZE: u64 = 4096;
/// Hardware D3D12 reserved-resource tile size (64 KiB).
pub const D3D12_TILE_SIZE: u64 = 65_536;

/// Explicit GGVL error type (no silent drops on invalid/uninitialized/overflowed accesses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GgvlError {
    UnknownResource(u64),
    AddressOverflow,
    OutOfBounds { addr: u64, len: u64, res_start: u64, res_end: u64 },
    NonResident(u64),
    InFlight(u64),
    UnknownTicket(u64),
}

/// Queue-aware GPU fence dependency vector.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueDeps {
    pub graphics: u64,
    pub compute: u64,
    pub copy: u64,
}

impl QueueDeps {
    pub fn merge(&mut self, other: QueueDeps) {
        self.graphics = self.graphics.max(other.graphics);
        self.compute = self.compute.max(other.compute);
        self.copy = self.copy.max(other.copy);
    }

    pub fn is_satisfied_by(&self, completed: QueueDeps) -> bool {
        self.graphics <= completed.graphics
            && self.compute <= completed.compute
            && self.copy <= completed.copy
    }
}

/// Per-4KiB-page authoritative ownership state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    Uninitialized,
    CpuAuthoritative,
    GpuAuthoritative,
    Coherent,
    CpuToGpuPending { ticket_id: u64 },
    GpuToCpuPending { ticket_id: u64 },
    DerivedConversionPending { deps: QueueDeps },
}

/// Per-4KiB-page metadata inside a tracked resource.
#[derive(Debug, Clone, Copy)]
pub struct PageMeta {
    pub state: PageState,
    pub cpu_gen: u64,
    pub gpu_gen: u64,
    pub last_gpu_deps: QueueDeps,
}

impl Default for PageMeta {
    fn default() -> Self {
        Self {
            state: PageState::Uninitialized,
            cpu_gen: 0,
            gpu_gen: 0,
            last_gpu_deps: QueueDeps::default(),
        }
    }
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

/// Kind of transactional transfer ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    CpuToGpu,
    GpuToCpu,
    DerivedConvert,
}

/// Generation-checked transfer ticket separating Plan -> Submit -> Complete/Cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferTicket {
    pub ticket_id: u64,
    pub resource_id: u64,
    pub kind: TransferKind,
    /// `(page_index, expected_cpu_gen, expected_gpu_gen)` captured at planning time.
    pub pages: Vec<(usize, u64, u64)>,
    pub submitted_deps: Option<QueueDeps>,
}

/// Synchronization plan returned by `plan_gpu_read` / `plan_cpu_read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPlan {
    /// No hash, compare, upload, or readback required.
    NoOp,
    /// Upload dirty guest byte ranges; caller must `submit_transfer` then `complete_transfer`.
    UploadRanges {
        ticket_id: u64,
        ranges: Vec<(u64, u64)>,
    },
    /// GPU-to-GPU barrier/fence only — zero CPU involvement.
    GpuBarrierOnly { deps: QueueDeps },
    /// Download GPU-authoritative byte ranges; caller must `submit_transfer` then `complete_transfer`.
    DownloadRanges {
        ticket_id: u64,
        ranges: Vec<(u64, u64)>,
        wait_deps: QueueDeps,
    },
    /// Re-derive aliased texture/RT representation on GPU.
    ConvertDerived {
        ticket_id: u64,
        wait_deps: QueueDeps,
    },
}

/// Per-resource metadata in the GGVL registry.
#[derive(Debug, Clone)]
pub struct GuestResource {
    pub id: u64,
    pub guest_addr: u64,
    pub size: u64,
    pub is_derived_texture: bool,
    pub priority: ResidencyPriority,
    pub resident_in_vram: bool,
    pub last_access_frame: u64,
    pub active_deps: QueueDeps,
    /// Per-4KiB-page ownership & generation table.
    pub pages: Vec<PageMeta>,
}

impl GuestResource {
    fn page_range(&self, addr: u64, len: u64) -> Result<(usize, usize), GgvlError> {
        let end = addr.checked_add(len).ok_or(GgvlError::AddressOverflow)?;
        let res_end = self.guest_addr.checked_add(self.size).ok_or(GgvlError::AddressOverflow)?;
        if len == 0 || addr < self.guest_addr || end > res_end {
            return Err(GgvlError::OutOfBounds {
                addr,
                len,
                res_start: self.guest_addr,
                res_end,
            });
        }
        let first = ((addr - self.guest_addr) / GUEST_PAGE_SIZE) as usize;
        let last = ((end - 1 - self.guest_addr) / GUEST_PAGE_SIZE) as usize;
        Ok((first, last))
    }
}

/// Sparse page table mapping 4 KiB guest pages to active resource IDs.
#[derive(Default)]
pub struct GuestMemoryPageTable {
    pages: BTreeMap<u64, Vec<u64>>,
}

impl GuestMemoryPageTable {
    pub fn register_range(&mut self, id: u64, guest_addr: u64, size: u64) -> Result<(), GgvlError> {
        let end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        if size == 0 {
            return Ok(());
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (end - 1) / GUEST_PAGE_SIZE;
        for p in first..=last {
            let entry = self.pages.entry(p).or_default();
            if !entry.contains(&id) {
                entry.push(id);
            }
        }
        Ok(())
    }

    pub fn unregister_range(&mut self, id: u64, guest_addr: u64, size: u64) {
        let Some(end) = guest_addr.checked_add(size) else {
            return;
        };
        if size == 0 {
            return;
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (end - 1) / GUEST_PAGE_SIZE;
        for p in first..=last {
            if let Some(entry) = self.pages.get_mut(&p) {
                entry.retain(|&x| x != id);
                if entry.is_empty() {
                    self.pages.remove(&p);
                }
            }
        }
    }

    pub fn overlapping(&self, guest_addr: u64, size: u64) -> Result<Vec<u64>, GgvlError> {
        let end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (end - 1) / GUEST_PAGE_SIZE;
        let mut out = Vec::new();
        for (_, ids) in self.pages.range(first..=last) {
            for &id in ids {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }
}

/// Event-driven, range-level, transactional coherency manager.
#[derive(Default)]
pub struct ResourceCoherencyManager {
    next_id: u64,
    next_ticket: u64,
    pub page_table: GuestMemoryPageTable,
    pub resources: HashMap<u64, GuestResource>,
    pub tickets: HashMap<u64, TransferTicket>,
    pub completed_deps: QueueDeps,
    pub skipped_hash_passes: u64,
}

impl ResourceCoherencyManager {
    pub fn register(
        &mut self,
        guest_addr: u64,
        size: u64,
        is_derived_texture: bool,
        priority: ResidencyPriority,
    ) -> Result<u64, GgvlError> {
        let _end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        self.next_id += 1;
        let id = self.next_id;
        self.page_table.register_range(id, guest_addr, size)?;
        let num_pages = if size == 0 {
            0
        } else {
            size.div_ceil(GUEST_PAGE_SIZE) as usize
        };
        self.resources.insert(
            id,
            GuestResource {
                id,
                guest_addr,
                size,
                is_derived_texture,
                priority,
                resident_in_vram: true,
                last_access_frame: 0,
                active_deps: QueueDeps::default(),
                pages: vec![PageMeta::default(); num_pages],
            },
        );
        Ok(id)
    }

    pub fn unregister(&mut self, id: u64) -> Result<(), GgvlError> {
        let res = self.resources.get(&id).ok_or(GgvlError::UnknownResource(id))?;
        if !res.active_deps.is_satisfied_by(self.completed_deps) {
            return Err(GgvlError::InFlight(id));
        }
        let (addr, size) = (res.guest_addr, res.size);
        self.page_table.unregister_range(id, addr, size);
        self.resources.remove(&id);
        Ok(())
    }

    /// Records a CPU write to `[guest_addr, guest_addr + size)` on exact intersecting pages only.
    /// Untouched GPU-authoritative pages in the same resource remain GPU-authoritative.
    pub fn on_cpu_write(&mut self, guest_addr: u64, size: u64) -> Result<usize, GgvlError> {
        let write_end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        let affected = self.page_table.overlapping(guest_addr, size)?;
        let mut pages_marked = 0usize;
        for id in affected {
            if let Some(res) = self.resources.get_mut(&id) {
                let res_end = res.guest_addr + res.size;
                let s = guest_addr.max(res.guest_addr);
                let e = write_end.min(res_end);
                if s >= e {
                    continue;
                }
                let first = ((s - res.guest_addr) / GUEST_PAGE_SIZE) as usize;
                let last = ((e - 1 - res.guest_addr) / GUEST_PAGE_SIZE) as usize;
                for pg in &mut res.pages[first..=last] {
                    pg.cpu_gen += 1;
                    pg.state = PageState::CpuAuthoritative;
                    pages_marked += 1;
                }
            }
        }
        Ok(pages_marked)
    }

    /// Records a GPU write to `[addr, addr + len)` on `id` with queue dependencies `deps`.
    /// Marks exact intersecting pages on `id` as `GpuAuthoritative` and overlapping aliases as
    /// `DerivedConversionPending` (textures) or `GpuAuthoritative` (buffers) without clobbering
    /// non-overlapping pages.
    pub fn on_gpu_write(
        &mut self,
        id: u64,
        addr: u64,
        len: u64,
        deps: QueueDeps,
    ) -> Result<(), GgvlError> {
        let res = self.resources.get(&id).ok_or(GgvlError::UnknownResource(id))?;
        let _ = res.page_range(addr, len)?;
        let write_end = addr + len;
        let aliases = self.page_table.overlapping(addr, len)?;
        for other_id in aliases {
            if let Some(other) = self.resources.get_mut(&other_id) {
                let other_end = other.guest_addr + other.size;
                let s = addr.max(other.guest_addr);
                let e = write_end.min(other_end);
                if s >= e {
                    continue;
                }
                other.active_deps.merge(deps);
                let first = ((s - other.guest_addr) / GUEST_PAGE_SIZE) as usize;
                let last = ((e - 1 - other.guest_addr) / GUEST_PAGE_SIZE) as usize;
                for pg in &mut other.pages[first..=last] {
                    pg.gpu_gen += 1;
                    pg.last_gpu_deps.merge(deps);
                    pg.state = if other_id == id {
                        PageState::GpuAuthoritative
                    } else if other.is_derived_texture {
                        PageState::DerivedConversionPending { deps }
                    } else {
                        PageState::GpuAuthoritative
                    };
                }
            }
        }
        Ok(())
    }

    /// Stage 1 (Plan): Plans lazy synchronization before a GPU read of `[addr, addr + len)`.
    /// Does NOT clear dirty state or mark pages coherent until `complete_transfer` is called.
    pub fn plan_gpu_read(
        &mut self,
        id: u64,
        addr: u64,
        len: u64,
        frame: u64,
    ) -> Result<SyncPlan, GgvlError> {
        let res = self.resources.get_mut(&id).ok_or(GgvlError::UnknownResource(id))?;
        if !res.resident_in_vram {
            return Err(GgvlError::NonResident(id));
        }
        let (first, last) = res.page_range(addr, len)?;
        res.last_access_frame = frame;

        let mut upload_pages = Vec::new();
        let mut derived_pages = Vec::new();
        let mut wait_deps = QueueDeps::default();
        let mut has_gpu_auth = false;

        for (idx, pg) in res.pages[first..=last].iter().enumerate() {
            let p = first + idx;
            match pg.state {
                PageState::CpuAuthoritative => {
                    upload_pages.push((p, pg.cpu_gen, pg.gpu_gen));
                }
                PageState::DerivedConversionPending { deps } => {
                    wait_deps.merge(deps);
                    derived_pages.push((p, pg.cpu_gen, pg.gpu_gen));
                }
                PageState::GpuAuthoritative => {
                    has_gpu_auth = true;
                    wait_deps.merge(pg.last_gpu_deps);
                }
                PageState::Coherent
                | PageState::Uninitialized
                | PageState::CpuToGpuPending { .. }
                | PageState::GpuToCpuPending { .. } => {}
            }
        }

        if !upload_pages.is_empty() {
            let mut ranges: Vec<(u64, u64)> = Vec::new();
            for &(p, _, _) in &upload_pages {
                let s = res.guest_addr + (p as u64) * GUEST_PAGE_SIZE;
                let e = (s + GUEST_PAGE_SIZE).min(res.guest_addr + res.size);
                if let Some((_, last_e)) = ranges.last_mut()
                    && *last_e == s
                {
                    *last_e = e;
                } else {
                    ranges.push((s, e));
                }
            }
            self.next_ticket += 1;
            let ticket_id = self.next_ticket;
            self.tickets.insert(
                ticket_id,
                TransferTicket {
                    ticket_id,
                    resource_id: id,
                    kind: TransferKind::CpuToGpu,
                    pages: upload_pages,
                    submitted_deps: None,
                },
            );
            return Ok(SyncPlan::UploadRanges { ticket_id, ranges });
        }

        if !derived_pages.is_empty() {
            self.next_ticket += 1;
            let ticket_id = self.next_ticket;
            self.tickets.insert(
                ticket_id,
                TransferTicket {
                    ticket_id,
                    resource_id: id,
                    kind: TransferKind::DerivedConvert,
                    pages: derived_pages,
                    submitted_deps: None,
                },
            );
            return Ok(SyncPlan::ConvertDerived { ticket_id, wait_deps });
        }

        self.skipped_hash_passes += 1;
        if has_gpu_auth {
            Ok(SyncPlan::GpuBarrierOnly { deps: wait_deps })
        } else {
            Ok(SyncPlan::NoOp)
        }
    }

    /// Stage 1 (Plan): Plans lazy synchronization before a CPU read of `[addr, addr + len)`.
    pub fn plan_cpu_read(&mut self, id: u64, addr: u64, len: u64) -> Result<SyncPlan, GgvlError> {
        let res = self.resources.get(&id).ok_or(GgvlError::UnknownResource(id))?;
        let (first, last) = res.page_range(addr, len)?;

        let mut download_pages = Vec::new();
        let mut wait_deps = QueueDeps::default();

        for (idx, pg) in res.pages[first..=last].iter().enumerate() {
            let p = first + idx;
            match pg.state {
                PageState::GpuAuthoritative | PageState::DerivedConversionPending { .. } => {
                    wait_deps.merge(pg.last_gpu_deps);
                    download_pages.push((p, pg.cpu_gen, pg.gpu_gen));
                }
                _ => {}
            }
        }

        if download_pages.is_empty() {
            return Ok(SyncPlan::NoOp);
        }

        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for &(p, _, _) in &download_pages {
            let s = res.guest_addr + (p as u64) * GUEST_PAGE_SIZE;
            let e = (s + GUEST_PAGE_SIZE).min(res.guest_addr + res.size);
            if let Some((_, last_e)) = ranges.last_mut()
                && *last_e == s
            {
                *last_e = e;
            } else {
                ranges.push((s, e));
            }
        }

        self.next_ticket += 1;
        let ticket_id = self.next_ticket;
        self.tickets.insert(
            ticket_id,
            TransferTicket {
                ticket_id,
                resource_id: id,
                kind: TransferKind::GpuToCpu,
                pages: download_pages,
                submitted_deps: None,
            },
        );
        Ok(SyncPlan::DownloadRanges {
            ticket_id,
            ranges,
            wait_deps,
        })
    }

    /// Stage 2 (Submit): Marks a planned transfer as submitted to GPU queues.
    pub fn submit_transfer(&mut self, ticket_id: u64, deps: QueueDeps) -> Result<(), GgvlError> {
        let ticket = self
            .tickets
            .get_mut(&ticket_id)
            .ok_or(GgvlError::UnknownTicket(ticket_id))?;
        ticket.submitted_deps = Some(deps);
        if let Some(res) = self.resources.get_mut(&ticket.resource_id) {
            res.active_deps.merge(deps);
            for &(p, exp_cpu, exp_gpu) in &ticket.pages {
                if let Some(pg) = res.pages.get_mut(p)
                    && pg.cpu_gen == exp_cpu
                    && pg.gpu_gen == exp_gpu
                {
                    pg.state = match ticket.kind {
                        TransferKind::CpuToGpu => PageState::CpuToGpuPending { ticket_id },
                        TransferKind::GpuToCpu => PageState::GpuToCpuPending { ticket_id },
                        TransferKind::DerivedConvert => PageState::DerivedConversionPending { deps },
                    };
                }
            }
        }
        Ok(())
    }

    /// Stage 3 (Complete): Marks pages `Coherent` ONLY if their generation still matches the
    /// snapshot captured at planning time (prevents losing concurrent CPU/GPU writes).
    pub fn complete_transfer(&mut self, ticket_id: u64) -> Result<usize, GgvlError> {
        let ticket = self
            .tickets
            .remove(&ticket_id)
            .ok_or(GgvlError::UnknownTicket(ticket_id))?;
        let mut committed = 0usize;
        if let Some(res) = self.resources.get_mut(&ticket.resource_id) {
            for &(p, exp_cpu, exp_gpu) in &ticket.pages {
                if let Some(pg) = res.pages.get_mut(p)
                    && pg.cpu_gen == exp_cpu
                    && pg.gpu_gen == exp_gpu
                {
                    pg.state = PageState::Coherent;
                    committed += 1;
                }
            }
        }
        Ok(committed)
    }

    /// Cancels a planned or failed transfer without corrupting authoritative state.
    pub fn cancel_transfer(&mut self, ticket_id: u64) -> Result<(), GgvlError> {
        let ticket = self
            .tickets
            .remove(&ticket_id)
            .ok_or(GgvlError::UnknownTicket(ticket_id))?;
        if let Some(res) = self.resources.get_mut(&ticket.resource_id) {
            for &(p, exp_cpu, exp_gpu) in &ticket.pages {
                if let Some(pg) = res.pages.get_mut(p)
                    && pg.cpu_gen == exp_cpu
                    && pg.gpu_gen == exp_gpu
                {
                    pg.state = match ticket.kind {
                        TransferKind::CpuToGpu => PageState::CpuAuthoritative,
                        TransferKind::GpuToCpu => PageState::GpuAuthoritative,
                        TransferKind::DerivedConvert => PageState::DerivedConversionPending {
                            deps: pg.last_gpu_deps,
                        },
                    };
                }
            }
        }
        Ok(())
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
    fn partial_cpu_write_preserves_untouched_gpu_authoritative_pages() {
        let mut mgr = ResourceCoherencyManager::default();
        let id = mgr.register(0x1000_0000, 16_384, false, ResidencyPriority::High).unwrap();
        let deps = QueueDeps { graphics: 5, compute: 0, copy: 0 };
        mgr.on_gpu_write(id, 0x1000_0000, 16_384, deps).unwrap();
        // CPU writes only 4 bytes in page 0; pages 1..=3 MUST stay GpuAuthoritative.
        mgr.on_cpu_write(0x1000_0000, 4).unwrap();
        let res = &mgr.resources[&id];
        assert_eq!(res.pages[0].state, PageState::CpuAuthoritative);
        assert_eq!(res.pages[1].state, PageState::GpuAuthoritative);
        assert_eq!(res.pages[2].state, PageState::GpuAuthoritative);
        assert_eq!(res.pages[3].state, PageState::GpuAuthoritative);
    }

    #[test]
    fn concurrent_cpu_write_during_upload_is_not_lost_on_complete() {
        let mut mgr = ResourceCoherencyManager::default();
        let id = mgr.register(0x2000_0000, 8192, false, ResidencyPriority::High).unwrap();
        mgr.on_cpu_write(0x2000_0000, 8192).unwrap();
        let SyncPlan::UploadRanges { ticket_id, ranges } =
            mgr.plan_gpu_read(id, 0x2000_0000, 8192, 1).unwrap()
        else {
            panic!("expected UploadRanges");
        };
        assert_eq!(ranges, vec![(0x2000_0000, 0x2000_2000)]);
        mgr.submit_transfer(ticket_id, QueueDeps { graphics: 0, compute: 0, copy: 1 }).unwrap();
        // Concurrent CPU write hits page 0 while upload is in flight!
        mgr.on_cpu_write(0x2000_0000, 16).unwrap();
        // Completing the old transfer commits page 1, but leaves page 0 CpuAuthoritative!
        let committed = mgr.complete_transfer(ticket_id).unwrap();
        assert_eq!(committed, 1);
        assert_eq!(mgr.resources[&id].pages[0].state, PageState::CpuAuthoritative);
        assert_eq!(mgr.resources[&id].pages[1].state, PageState::Coherent);
    }

    #[test]
    fn cancelled_upload_preserves_cpu_authoritative_state() {
        let mut mgr = ResourceCoherencyManager::default();
        let id = mgr.register(0x3000_0000, 4096, false, ResidencyPriority::High).unwrap();
        mgr.on_cpu_write(0x3000_0000, 4096).unwrap();
        let SyncPlan::UploadRanges { ticket_id, .. } =
            mgr.plan_gpu_read(id, 0x3000_0000, 4096, 1).unwrap()
        else {
            panic!("expected UploadRanges");
        };
        mgr.submit_transfer(ticket_id, QueueDeps { graphics: 0, compute: 0, copy: 2 }).unwrap();
        mgr.cancel_transfer(ticket_id).unwrap();
        assert_eq!(mgr.resources[&id].pages[0].state, PageState::CpuAuthoritative);
    }

    #[test]
    fn partial_cpu_readback_only_marks_read_range_coherent() {
        let mut mgr = ResourceCoherencyManager::default();
        let id = mgr.register(0x4000_0000, 8192, false, ResidencyPriority::High).unwrap();
        let deps = QueueDeps { graphics: 10, compute: 0, copy: 0 };
        mgr.on_gpu_write(id, 0x4000_0000, 8192, deps).unwrap();
        let SyncPlan::DownloadRanges { ticket_id, ranges, wait_deps } =
            mgr.plan_cpu_read(id, 0x4000_0000, 512).unwrap()
        else {
            panic!("expected DownloadRanges");
        };
        assert_eq!(ranges, vec![(0x4000_0000, 0x4000_1000)]);
        assert_eq!(wait_deps, deps);
        mgr.complete_transfer(ticket_id).unwrap();
        assert_eq!(mgr.resources[&id].pages[0].state, PageState::Coherent);
        assert_eq!(mgr.resources[&id].pages[1].state, PageState::GpuAuthoritative);
    }

    #[test]
    fn unknown_and_oob_accesses_return_explicit_errors() {
        let mut mgr = ResourceCoherencyManager::default();
        assert_eq!(mgr.plan_gpu_read(999, 0, 64, 1), Err(GgvlError::UnknownResource(999)));
        let id = mgr.register(0x5000_0000, 4096, false, ResidencyPriority::High).unwrap();
        assert!(matches!(
            mgr.plan_gpu_read(id, 0x5000_0000, 8192, 1),
            Err(GgvlError::OutOfBounds { .. })
        ));
        assert_eq!(mgr.on_cpu_write(u64::MAX, 16), Err(GgvlError::AddressOverflow));
    }

    #[test]
    fn multi_queue_fence_eviction_and_aliasing() {
        let mut mgr = ResourceCoherencyManager::default();
        mgr.completed_deps = QueueDeps { graphics: 10, compute: 5, copy: 2 };
        let buf = mgr.register(0x6000_0000, 8192, false, ResidencyPriority::Low).unwrap();
        let tex = mgr.register(0x6000_0000, 8192, true, ResidencyPriority::High).unwrap();
        // Compute queue write on fence 8 (> completed 5) blocks eviction & marks aliased tex derived.
        mgr.on_gpu_write(buf, 0x6000_0000, 4096, QueueDeps { graphics: 0, compute: 8, copy: 0 }).unwrap();
        assert_eq!(mgr.unregister(buf), Err(GgvlError::InFlight(buf)));
        let mut res_mgr = ResidencyManager::new(10_000);
        res_mgr.update_budget(10_000, 9500);
        assert!(res_mgr.select_evictions(&mgr).is_empty());
        assert!(matches!(
            mgr.plan_gpu_read(tex, 0x6000_0000, 4096, 2).unwrap(),
            SyncPlan::ConvertDerived { .. }
        ));
    }
}
