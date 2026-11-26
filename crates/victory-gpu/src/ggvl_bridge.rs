//! Live bridge wiring hardened `versus_backend::ggvl` (Milestone 1.1) into `victory-gpu`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use versus_backend::ggvl::{
    GgvlError, QueueDeps, ResidencyManager, ResidencyPriority, ResourceCoherencyManager, SyncPlan,
};

struct GgvlState {
    coherency: ResourceCoherencyManager,
    residency: ResidencyManager,
    by_title_buf: HashMap<(u64, usize), u64>,
}

static STATE: LazyLock<Mutex<GgvlState>> = LazyLock::new(|| {
    Mutex::new(GgvlState {
        coherency: ResourceCoherencyManager::default(),
        residency: ResidencyManager::new(6 * 1024 * 1024 * 1024),
        by_title_buf: HashMap::new(),
    })
});

static BUDGET_UPDATES: AtomicU64 = AtomicU64::new(0);
static VRAM_PRESSURE: AtomicBool = AtomicBool::new(false);
static CPU_WRITE_EVENTS: AtomicU64 = AtomicU64::new(0);
static GPU_WRITE_EVENTS: AtomicU64 = AtomicU64::new(0);
static READONLY_HASH_SKIPS: AtomicU64 = AtomicU64::new(0);
static EVICTED_BUFFERS: AtomicU64 = AtomicU64::new(0);
static EVICTED_BYTES: AtomicU64 = AtomicU64::new(0);

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
        }
        if n > 0 && n % 200 == 0 {
            // The sorted candidate list is telemetry only. Do not rebuild it
            // on every budget query; reclamation uses the independent pressure bit.
            let evict = st.residency.select_evictions(&st.coherency);
            tracing::info!(
                pressure,
                budget_mb = budget_bytes / (1024 * 1024),
                usage_mb = usage_bytes / (1024 * 1024),
                tracked = st.coherency.resources.len(),
                cpu_writes = CPU_WRITE_EVENTS.load(Ordering::Relaxed),
                gpu_writes = GPU_WRITE_EVENTS.load(Ordering::Relaxed),
                readonly_hash_skips = READONLY_HASH_SKIPS.load(Ordering::Relaxed),
                evict_candidates = evict.len(),
                evicted_buffers = EVICTED_BUFFERS.load(Ordering::Relaxed),
                evicted_mb = EVICTED_BYTES.load(Ordering::Relaxed) / (1024 * 1024),
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
    if let Ok(id) = st.coherency.register(start, len, false, priority) {
        st.by_title_buf.insert((start, resource), id);
    }
}

/// Unregisters a title buffer from `shadow::unregister` (defers if GPU work is in flight).
pub(crate) fn on_buffer_unregister(resource: usize, start: u64) {
    if !enabled() {
        return;
    }
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(id) = st.by_title_buf.remove(&(start, resource)) {
        let _ = st.coherency.unregister(id);
    }
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
    st.coherency.completed_deps.graphics =
        st.coherency.completed_deps.graphics.max(completed);
}

/// Returns true if all GGVL queue fence dependencies on `guest_addr..guest_addr+len`
/// are satisfied by `completed_deps`, meaning eviction is fence-safe.
pub(crate) fn is_fence_safe(guest_addr: u64, len: u64) -> bool {
    if !enabled() || len == 0 {
        return true;
    }
    let st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(ids) = st.coherency.page_table.overlapping(guest_addr, len) else {
        return false;
    };
    ids.iter().all(|id| {
        st.coherency
            .resources
            .get(id)
            .is_none_or(|r| r.active_deps.is_satisfied_by(st.coherency.completed_deps))
    })
}

/// Records an actual VRAM eviction of a cold buffer (marks nonresident until restored).
pub(crate) fn note_evicted(guest_addr: u64, len: u64) {
    EVICTED_BUFFERS.fetch_add(1, Ordering::Relaxed);
    EVICTED_BYTES.fetch_add(len, Ordering::Relaxed);
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(ids) = st.coherency.page_table.overlapping(guest_addr, len) {
        for id in ids {
            if let Some(res) = st.coherency.resources.get_mut(&id) {
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

/// Records a live CPU->GPU dirty upload range from `host.rs`.
pub(crate) fn note_cpu_upload(guest_addr: u64, len: u64) {
    if !enabled() || len == 0 {
        return;
    }
    CPU_WRITE_EVENTS.fetch_add(1, Ordering::Relaxed);
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let _ = st.coherency.on_cpu_write(guest_addr, len);
}

/// Records a live declared UAV GPU write range from `host.rs`.
pub(crate) fn note_gpu_uav_write(guest_addr: u64, len: u64, fence: u64) {
    if !enabled() || len == 0 {
        return;
    }
    GPU_WRITE_EVENTS.fetch_add(1, Ordering::Relaxed);
    let mut st = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(ids) = st.coherency.page_table.overlapping(guest_addr, len) {
        let deps = QueueDeps { graphics: fence, compute: 0, copy: 0 };
        for id in ids {
            if let Some(res) = st.coherency.resources.get(&id) {
                let s = guest_addr.max(res.guest_addr);
                let e = guest_addr.saturating_add(len).min(res.guest_addr + res.size);
                if s < e {
                    let _ = st.coherency.on_gpu_write(id, s, e - s, deps);
                }
            }
        }
    }
}

/// Returns true if `guest_addr..guest_addr+len` has never been GPU-written and
/// `VICTORY_GGVL_SKIP_READONLY_HASH=1` is enabled.
pub(crate) fn may_skip_rehash(guest_addr: u64, len: u64) -> bool {
    if !enabled() || !skip_readonly_hash() || len == 0 {
        return false;
    }
    if let Ok(st) = STATE.try_lock()
        && let Ok(ids) = st.coherency.page_table.overlapping(guest_addr, len)
        && !ids.is_empty()
    {
        let any_gpu_written = ids.iter().any(|id| {
            st.coherency
                .resources
                .get(id)
                .is_some_and(|r| r.active_deps.graphics > 0 || r.active_deps.compute > 0)
        });
        if !any_gpu_written {
            READONLY_HASH_SKIPS.fetch_add(1, Ordering::Relaxed);
            return true;
        }
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
