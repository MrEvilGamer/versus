//! Live bridge wiring hardened `versus_backend::ggvl` (Milestone 1.1) into `victory-gpu`.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use versus_backend::ggvl::{
    AdaptiveVramController, AllocationId, AllocationKind, AllocationRegistry, BackingStorageId,
    GgvlError, MemorySegment, OwnershipState, PageState, QueueDeps, ReclaimTier, ResidencyManager,
    ResidencyPriority, ResourceCoherencyManager, SyncPlan, TrackedAllocation,
};

struct GgvlState {
    coherency: ResourceCoherencyManager,
    residency: ResidencyManager,
    registry: AllocationRegistry,
    controller: AdaptiveVramController,
    by_title_buf: HashMap<(u64, usize), u64>,
    pending_unregister: HashSet<u64>,
}

impl GgvlState {
    fn new() -> Self {
        Self {
            coherency: ResourceCoherencyManager::default(),
            residency: ResidencyManager::new(6 * 1024 * 1024 * 1024),
            registry: AllocationRegistry::new(),
            controller: AdaptiveVramController::new(6 * 1024 * 1024 * 1024, 128 << 20),
            by_title_buf: HashMap::new(),
            pending_unregister: HashSet::new(),
        }
    }

    fn retire_registration(&mut self, id: u64) {
        match self.coherency.unregister(id) {
            Ok(()) | Err(GgvlError::UnknownResource(_)) => {
                self.pending_unregister.remove(&id);
                self.registry.unregister(AllocationId(id));
            }
            Err(_) => {
                self.pending_unregister.insert(id);
            }
        }
    }

    fn register_buffer(&mut self, key: (u64, usize), len: u64, priority: ResidencyPriority) {
        // A reused title object/address must not orphan its previous ID.
        if let Some(old) = self.by_title_buf.remove(&key) {
            self.retire_registration(old);
        }
        if let Ok(id) = self.coherency.register(key.0, len, false, priority) {
            self.by_title_buf.insert(key, id);
            self.registry.register(TrackedAllocation {
                id: AllocationId(id),
                backing_id: BackingStorageId(key.0),
                kind: AllocationKind::ShadowBuffer,
                size_bytes: len,
                segment: MemorySegment::SystemRam,
                last_access_frame: 0,
                access_frequency: 1,
                ownership: OwnershipState::CpuAuthoritative,
                descriptor_refs: 0,
                generation: 1,
                active_deps: QueueDeps::default(),
                restoration_cost: 20,
                evictable: priority < ResidencyPriority::Critical,
                resident: true,
                reclaim_tier: Some(ReclaimTier::ColdShadowBuffer),
            });
        }
    }

    fn unregister_buffer(&mut self, key: (u64, usize)) {
        if let Some(id) = self.by_title_buf.remove(&key) {
            self.retire_registration(id);
        }
    }

    fn complete_graphics(&mut self, completed: u64) {
        self.coherency.completed_deps.graphics =
            self.coherency.completed_deps.graphics.max(completed);
        let coherency = &mut self.coherency;
        self.pending_unregister.retain(|id| {
            !matches!(coherency.unregister(*id), Ok(()) | Err(GgvlError::UnknownResource(_)))
        });
    }

    // This is deliberately conservative: page-index hits are candidates,
    // not proof of exact byte coverage or of ownership. Only a live
    // containing registration, with no observed GPU-write history in any
    // overlapping alias, is eligible for the experimental shortcut.
    // For destruction also require a single exact registration.
    fn known_readonly_range(&self, addr: u64, len: u64, exact_single: bool) -> bool {
        let Some(end) = addr.checked_add(len) else { return false; };
        if len == 0 { return false; }
        let Ok(ids) = self.coherency.page_table.overlapping(addr, len) else { return false; };
        let mut contains = false;
        let mut overlapping = 0usize;
        for id in ids {
            let Some(res) = self.coherency.resources.get(&id) else { return false; };
            let Some(res_end) = res.guest_addr.checked_add(res.size) else { return false; };
            if addr >= res_end || end <= res.guest_addr { continue; }
            overlapping += 1;
            if self.pending_unregister.contains(&id)
                || !res.resident_in_vram
                || res.is_derived_texture
                || res.active_deps != QueueDeps::default()
                || self.coherency.tickets.values().any(|ticket| ticket.resource_id == id)
            {
                return false;
            }
            let first = ((addr.max(res.guest_addr) - res.guest_addr)
                / versus_backend::ggvl::GUEST_PAGE_SIZE) as usize;
            let last = ((end.min(res_end) - 1 - res.guest_addr)
                / versus_backend::ggvl::GUEST_PAGE_SIZE) as usize;
            let Some(pages) = res.pages.get(first..=last) else { return false; };
            if pages.iter().any(|p| p.gpu_gen != 0 || p.last_gpu_deps != QueueDeps::default()
                || (exact_single && !matches!(p.state, PageState::CpuAuthoritative | PageState::Coherent))
                || matches!(p.state, PageState::GpuAuthoritative | PageState::DerivedConversionPending { .. }
                    | PageState::CpuToGpuPending { .. } | PageState::GpuToCpuPending { .. }))
            {
                return false;
            }
            contains |= if exact_single {
                res.guest_addr == addr && res_end == end
            } else {
                res.guest_addr <= addr && res_end >= end
            };
        }
        contains && (!exact_single || overlapping == 1)
    }
}

static STATE: LazyLock<Mutex<GgvlState>> = LazyLock::new(|| Mutex::new(GgvlState::new()));

static BUDGET_UPDATES: AtomicU64 = AtomicU64::new(0);
static VRAM_PRESSURE: AtomicBool = AtomicBool::new(false);
static CPU_WRITE_EVENTS: AtomicU64 = AtomicU64::new(0);
static GPU_WRITE_EVENTS: AtomicU64 = AtomicU64::new(0);
static READONLY_HASH_SKIPS: AtomicU64 = AtomicU64::new(0);
static EVICTED_BUFFERS: AtomicU64 = AtomicU64::new(0);
static EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);
static LAST_BUDGET_BYTES: AtomicU64 = AtomicU64::new(0);
static LAST_USAGE_BYTES: AtomicU64 = AtomicU64::new(0);
static RECLAIM_REQUIRED_BYTES: AtomicU64 = AtomicU64::new(0);
static NEXT_HEAP_ID: AtomicU64 = AtomicU64::new(1);
static TEX_HEAP_BYTES: AtomicU64 = AtomicU64::new(0);
static SHADOW_BUF_BYTES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn set_local_shadow_bytes(bytes: u64) {
    SHADOW_BUF_BYTES.store(bytes, Ordering::Relaxed);
}

pub(crate) fn vram_snapshot_mb() -> (u64, u64) {
    (
        LAST_BUDGET_BYTES.load(Ordering::Relaxed) >> 20,
        LAST_USAGE_BYTES.load(Ordering::Relaxed) >> 20,
    )
}

/// Registers a newly created, resident physical texture pool heap with a stable monotonic ID.
pub(crate) fn register_texture_heap(size_bytes: u64) -> u64 {
    let heap_id = NEXT_HEAP_ID.fetch_add(1, Ordering::Relaxed);
    if !enabled() {
        return heap_id;
    }
    TEX_HEAP_BYTES.fetch_add(size_bytes, Ordering::Relaxed);
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let id = 0x8000_0000_0000_0000u64 | heap_id;
    st.registry.register(TrackedAllocation {
        id: AllocationId(id),
        backing_id: BackingStorageId(id),
        kind: AllocationKind::TextureHeap,
        size_bytes,
        segment: MemorySegment::LocalVram,
        last_access_frame: 0,
        access_frequency: 1,
        ownership: OwnershipState::Coherent,
        descriptor_refs: 0,
        generation: 1,
        active_deps: QueueDeps::default(),
        restoration_cost: 0,
        evictable: true,
        resident: true,
        reclaim_tier: Some(ReclaimTier::EmptyHeap),
    });
    heap_id
}

/// Unregisters a retired physical texture pool heap by its original monotonic ID.
/// Derives released bytes from the removed registry record so double-unregister is a no-op.
pub(crate) fn unregister_texture_heap(heap_id: u64) -> u64 {
    if !enabled() || heap_id == 0 {
        return 0;
    }
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let id = 0x8000_0000_0000_0000u64 | heap_id;
    if let Some(removed) = st.registry.unregister(AllocationId(id)) {
        TEX_HEAP_BYTES.fetch_sub(
            removed.size_bytes.min(TEX_HEAP_BYTES.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        removed.size_bytes
    } else {
        0
    }
}

/// Whether GGVL tracking is active (`VICTORY_GGVL=0` disables it).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GGVL").is_none_or(|v| v != "0"))
}

/// Opt-in Milestone 3: skip GPU re-hash passes for buffers that have never been UAV-written.
pub(crate) fn skip_readonly_hash() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GGVL_SKIP_READONLY_HASH").is_some_and(|v| v == "1"))
}

/// Pressure follows the budget, not the number of eligible shadow buffers.
/// Empty texture heaps can be reclaimed even when every shadow is in flight.
/// Retain the previous state on an invalid (zero-budget) sample.
fn pressure_after_sample(previous: bool, budget: u64, usage: u64, high: u64, low: u64) -> bool {
    if budget == 0 {
        return previous;
    }
    let used = u128::from(usage) * 100;
    let threshold = u128::from(budget) * u128::from(if previous { low } else { high });
    used > threshold
}

/// Feeds live `IDXGIAdapter3::QueryVideoMemoryInfo` local budget & usage into `ResidencyManager`.
pub(crate) fn on_vram_budget(budget_bytes: u64, usage_bytes: u64) {
    if !enabled() {
        return;
    }
    let n = BUDGET_UPDATES.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut st) = STATE.lock() {
        let pressure = pressure_after_sample(
            VRAM_PRESSURE.load(Ordering::Relaxed),
            budget_bytes,
            usage_bytes,
            st.residency.high_watermark_pct,
            st.residency.low_watermark_pct,
        );
        VRAM_PRESSURE.store(pressure, Ordering::Relaxed);
        if budget_bytes != 0 {
            st.residency.update_budget(budget_bytes, usage_bytes);
            st.controller.update(budget_bytes, usage_bytes);
            LAST_BUDGET_BYTES.store(budget_bytes, Ordering::Relaxed);
            LAST_USAGE_BYTES.store(usage_bytes, Ordering::Relaxed);
            RECLAIM_REQUIRED_BYTES.store(st.controller.reclamation_required_bytes(), Ordering::Relaxed);
        }
        if n > 0 && n % 200 == 0 {
            // The sorted candidate list is telemetry only. Do not rebuild it
            // on every budget query; reclamation uses the independent pressure bit.
            let evict = st.residency.select_evictions(&st.coherency);
            let total_tracked_mb = st.registry.total_unique_backing_bytes() >> 20;
            let tex_heap_mb = TEX_HEAP_BYTES.load(Ordering::Relaxed) >> 20;
            let local_shadow_mb = SHADOW_BUF_BYTES.load(Ordering::Relaxed) >> 20;
            let sys_shadow_mb = total_tracked_mb.saturating_sub(tex_heap_mb + local_shadow_mb);
            let local_tracked_mb = tex_heap_mb + local_shadow_mb;
            let reclaim_req_mb = st.controller.reclamation_required_bytes() >> 20;
            let usage_mb = usage_bytes >> 20;
            let rt_depth_mb = crate::resource::target_bytes() >> 20;
            let filled_tex_mb = crate::resource::filled_bytes() >> 20;
            let unaccounted_local_mb = usage_mb.saturating_sub(local_tracked_mb + rt_depth_mb);
            tracing::info!(
                pressure,
                budget_mb = budget_bytes >> 20,
                usage_mb,
                local_tracked_mb,
                tex_heap_mb,
                filled_tex_mb,
                rt_depth_mb,
                local_shadow_mb,
                sys_shadow_mb,
                unaccounted_local_mb,
                reclaim_req_mb,
                tracked = st.coherency.resources.len(),
                cpu_writes = CPU_WRITE_EVENTS.load(Ordering::Relaxed),
                gpu_writes = GPU_WRITE_EVENTS.load(Ordering::Relaxed),
                readonly_hash_skips = READONLY_HASH_SKIPS.load(Ordering::Relaxed),
                evict_candidates = evict.len(),
                evicted_buffers = EVICTED_BUFFERS.load(Ordering::Relaxed),
                evicted_mb = EVICTED_BYTES.load(Ordering::Relaxed) >> 20,
                "ggvl: live residency & coherency status"
            );
        }
    }
}

/// Registers a title buffer from `shadow::register` into the live GGVL address space.
pub(crate) fn on_buffer_register(resource: usize, start: u64, len: u64) {
    if !enabled() || len == 0 {
        return;
    }
    let priority = if len >= (1 << 20) {
        ResidencyPriority::High
    } else {
        ResidencyPriority::Medium
    };
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.register_buffer((start, resource), len, priority);
}

/// Unregisters a title buffer from `shadow::unregister` (defers if GPU work is in flight).
pub(crate) fn on_buffer_unregister(resource: usize, start: u64) {
    if !enabled() {
        return;
    }
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.unregister_buffer((start, resource));
}

/// Enters pressure above the high watermark (92%), exits at or below the low
/// watermark (82%), independently of whether shadow eviction is possible.
pub(crate) fn vram_under_pressure() -> bool {
    enabled() && VRAM_PRESSURE.load(Ordering::Relaxed)
}

/// Advances the completed graphics fence so fence-safe eviction & deferred destruction stay exact.
pub(crate) fn on_graphics_fence_complete(completed: u64) {
    if !enabled() {
        return;
    }
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.complete_graphics(completed);
}

/// Conservative eligibility for destroying a regenerable shadow. A missing
/// registration or an alias with unknown provenance is not proof of safety.
/// The caller must also establish host resource lifetime and descriptor safety.
pub(crate) fn is_fence_safe(guest_addr: u64, len: u64) -> bool {
    if !enabled() || len == 0 {
        return false;
    }
    let st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    st.known_readonly_range(guest_addr, len, true)
}

/// Records an actual VRAM eviction of a cold buffer (marks nonresident until restored).
pub(crate) fn note_evicted(guest_addr: u64, len: u64) {
    EVICTED_BUFFERS.fetch_add(1, Ordering::Relaxed);
    EVICTED_BYTES.fetch_add(len, Ordering::Relaxed);
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(ids) = st.coherency.page_table.overlapping(guest_addr, len) {
        for id in ids {
            if let Some(res) = st.coherency.resources.get_mut(&id)
                && res.guest_addr == guest_addr && res.size == len
            {
                res.resident_in_vram = false;
            }
        }
    }
}

/// Records exact disjoint CPU->GPU dirty upload ranges for one buffer under a single lock.
pub(crate) fn note_cpu_uploads(base_addr: u64, ranges: &[(usize, usize)]) {
    if !enabled() || ranges.is_empty() {
        return;
    }
    CPU_WRITE_EVENTS.fetch_add(ranges.len() as u64, Ordering::Relaxed);
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    for &(a, b) in ranges {
        let len = b.saturating_sub(a) as u64;
        if len > 0 {
            let _ = st.coherency.on_cpu_write(base_addr.saturating_add(a as u64), len);
        }
    }
}

/// Records a live declared UAV GPU write range from `host.rs`.
pub(crate) fn note_gpu_uav_write(guest_addr: u64, len: u64, fence: u64) {
    if !enabled() || len == 0 {
        return;
    }
    GPU_WRITE_EVENTS.fetch_add(1, Ordering::Relaxed);
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let deps = QueueDeps { graphics: fence, compute: 0, copy: 0 };
    let _ = st.coherency.on_gpu_buffer_write(guest_addr, len, deps);
}

/// Returns true for a fully covered range with no observed GPU-write history, and
/// `VICTORY_GGVL_SKIP_READONLY_HASH=1` is enabled.
pub(crate) fn may_skip_rehash(guest_addr: u64, len: u64) -> bool {
    if !enabled() || !skip_readonly_hash() || len == 0 {
        return false;
    }
    if let Ok(st) = STATE.try_lock()
        && st.known_readonly_range(guest_addr, len, false)
    {
        READONLY_HASH_SKIPS.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    false
}

#[allow(dead_code)]
pub(crate) fn register(
    guest_addr: u64,
    size: u64,
    is_derived_texture: bool,
    priority: ResidencyPriority,
) -> Result<u64, GgvlError> {
    if !enabled() {
        return Ok(0);
    }
    STATE
        .lock()
        .map_err(|_| GgvlError::UnknownResource(0))?
        .coherency
        .register(guest_addr, size, is_derived_texture, priority)
}

#[allow(dead_code)]
pub(crate) fn on_cpu_write(guest_addr: u64, size: u64) -> Result<usize, GgvlError> {
    if !enabled() {
        return Ok(0);
    }
    STATE
        .lock()
        .map_err(|_| GgvlError::UnknownResource(0))?
        .coherency
        .on_cpu_write(guest_addr, size)
}

#[allow(dead_code)]
pub(crate) fn on_gpu_write(id: u64, addr: u64, len: u64, deps: QueueDeps) -> Result<(), GgvlError> {
    if !enabled() || id == 0 {
        return Ok(());
    }
    STATE
        .lock()
        .map_err(|_| GgvlError::UnknownResource(id))?
        .coherency
        .on_gpu_write(id, addr, len, deps)
}

#[allow(dead_code)]
pub(crate) fn plan_gpu_read(id: u64, addr: u64, len: u64, frame: u64) -> Result<SyncPlan, GgvlError> {
    if !enabled() || id == 0 {
        return Ok(SyncPlan::NoOp);
    }
    STATE
        .lock()
        .map_err(|_| GgvlError::UnknownResource(id))?
        .coherency
        .plan_gpu_read(id, addr, len, frame)
}

#[cfg(test)]
mod pressure_tests {
    use super::pressure_after_sample;

    #[test]
    fn pressure_uses_hysteresis_without_candidates() {
        assert!(!pressure_after_sample(false, 100, 92, 92, 82));
        assert!(pressure_after_sample(false, 100, 93, 92, 82));
        assert!(pressure_after_sample(true, 100, 90, 92, 82));
        assert!(pressure_after_sample(true, 100, 83, 92, 82));
        assert!(!pressure_after_sample(true, 100, 82, 92, 82));
        assert!(!pressure_after_sample(false, 100, 90, 92, 82));
    }

    #[test]
    fn pressure_handles_budget_changes_and_invalid_samples() {
        assert!(pressure_after_sample(false, 80, 80, 92, 82));
        assert!(!pressure_after_sample(true, 100, 80, 92, 82));
        assert!(pressure_after_sample(true, 0, 0, 92, 82));
        assert!(!pressure_after_sample(false, 0, u64::MAX, 92, 82));
        assert!(pressure_after_sample(false, u64::MAX, u64::MAX, 92, 82));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn unregister_retries_after_submission_finishes() {
        let mut st = GgvlState::new();
        let key = (0x1000, 1);
        st.register_buffer(key, 4096, ResidencyPriority::High);
        let id = st.by_title_buf[&key];
        st.coherency.on_gpu_write(id, key.0, 4096, QueueDeps { graphics: 5, ..Default::default() }).unwrap();
        st.unregister_buffer(key);
        assert!(!st.by_title_buf.contains_key(&key));
        assert!(st.pending_unregister.contains(&id));
        st.complete_graphics(4);
        assert!(st.coherency.resources.contains_key(&id));
        st.complete_graphics(5);
        assert!(!st.coherency.resources.contains_key(&id));
        assert!(st.pending_unregister.is_empty());
        assert!(st.coherency.page_table.overlapping(key.0, 4096).unwrap().is_empty());
    }

    #[test]
    fn replaced_registration_does_not_orphan_previous_id() {
        let mut st = GgvlState::new();
        let key = (0x2000, 2);
        st.register_buffer(key, 4096, ResidencyPriority::High);
        let first = st.by_title_buf[&key];
        st.coherency.on_gpu_write(first, key.0, 4096, QueueDeps { graphics: 3, ..Default::default() }).unwrap();
        st.register_buffer(key, 4096, ResidencyPriority::High);
        let second = st.by_title_buf[&key];
        assert_ne!(first, second);
        assert!(st.pending_unregister.contains(&first));
        assert!(!st.known_readonly_range(key.0, 4096, false));
        st.complete_graphics(3);
        assert!(!st.coherency.resources.contains_key(&first));
        assert_eq!(st.by_title_buf[&key], second);
        assert!(st.coherency.resources.contains_key(&second));
    }

    #[test]
    fn range_guards_require_coverage_and_ignore_unrelated_page_hits() {
        let mut st = GgvlState::new();
        st.register_buffer((0x1000, 1), 16, ResidencyPriority::High);
        st.register_buffer((0x1800, 2), 16, ResidencyPriority::High);
        assert!(!st.known_readonly_range(0x1000, 32, false));
        assert!(!st.known_readonly_range(0x1100, 1, false));
        assert!(!st.known_readonly_range(u64::MAX, 1, false));
        assert!(st.known_readonly_range(0x1000, 16, false));
        assert!(!st.known_readonly_range(0x1000, 16, true));
        st.coherency.on_cpu_write(0x1000, 16).unwrap();
        assert!(st.known_readonly_range(0x1000, 16, true));
        st.coherency.on_gpu_buffer_write(0x1800, 16, QueueDeps { copy: 1, ..Default::default() }).unwrap();
        assert!(st.known_readonly_range(0x1000, 16, false));
        assert!(!st.known_readonly_range(0x1800, 16, false));
        st.register_buffer((0x1008, 3), 8, ResidencyPriority::High);
        assert!(!st.known_readonly_range(0x1000, 16, true));
        st.coherency.on_gpu_buffer_write(0x1008, 1, QueueDeps::default()).unwrap();
        assert!(!st.known_readonly_range(0x1000, 16, false));
    }

    #[test]
    fn texture_heap_registration_retirement_and_duplicate_unregister() {
        let before = TEX_HEAP_BYTES.load(Ordering::Relaxed);
        let id1 = register_texture_heap(64 << 20);
        let id2 = register_texture_heap(64 << 20);
        assert_ne!(id1, id2);
        assert_eq!(TEX_HEAP_BYTES.load(Ordering::Relaxed), before + (128 << 20));
        assert_eq!(unregister_texture_heap(id1), 64 << 20);
        assert_eq!(TEX_HEAP_BYTES.load(Ordering::Relaxed), before + (64 << 20));
        // Duplicate unregister must be a safe no-op returning 0.
        assert_eq!(unregister_texture_heap(id1), 0);
        assert_eq!(TEX_HEAP_BYTES.load(Ordering::Relaxed), before + (64 << 20));
        assert_eq!(unregister_texture_heap(id2), 64 << 20);
        assert_eq!(TEX_HEAP_BYTES.load(Ordering::Relaxed), before);
    }
}
