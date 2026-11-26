//! Live bridge wiring hardened `versus_backend::ggvl` (Milestone 1.1) into `victory-gpu`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

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
static LAST_EVICT_CANDIDATES: AtomicU64 = AtomicU64::new(0);
static CPU_WRITE_EVENTS: AtomicU64 = AtomicU64::new(0);
static GPU_WRITE_EVENTS: AtomicU64 = AtomicU64::new(0);
static READONLY_HASH_SKIPS: AtomicU64 = AtomicU64::new(0);

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

/// Feeds live `IDXGIAdapter3::QueryVideoMemoryInfo` local budget & usage into `ResidencyManager`.
pub(crate) fn on_vram_budget(budget_bytes: u64, usage_bytes: u64) {
    if !enabled() {
        return;
    }
    let n = BUDGET_UPDATES.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut st) = STATE.try_lock() {
        st.residency.update_budget(budget_bytes, usage_bytes);
        let evict = st.residency.select_evictions(&st.coherency);
        LAST_EVICT_CANDIDATES.store(evict.len() as u64, Ordering::Relaxed);
        if n > 0 && n % 200 == 0 {
            tracing::info!(
                budget_mb = budget_bytes / (1024 * 1024),
                usage_mb = usage_bytes / (1024 * 1024),
                tracked = st.coherency.resources.len(),
                cpu_writes = CPU_WRITE_EVENTS.load(Ordering::Relaxed),
                gpu_writes = GPU_WRITE_EVENTS.load(Ordering::Relaxed),
                readonly_hash_skips = READONLY_HASH_SKIPS.load(Ordering::Relaxed),
                evict_candidates = evict.len(),
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
    if let Ok(mut st) = STATE.try_lock()
        && let Ok(id) = st.coherency.register(start, len, false, priority)
    {
        st.by_title_buf.insert((start, resource), id);
    }
}

/// Unregisters a title buffer from `shadow::unregister` (defers if GPU work is in flight).
pub(crate) fn on_buffer_unregister(resource: usize, start: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut st) = STATE.try_lock()
        && let Some(id) = st.by_title_buf.remove(&(start, resource))
    {
        let _ = st.coherency.unregister(id);
    }
}

/// Returns true when live DXGI VRAM usage exceeds `ResidencyManager`'s high watermark (92%).
pub(crate) fn vram_under_pressure() -> bool {
    enabled() && LAST_EVICT_CANDIDATES.load(Ordering::Relaxed) > 0
}

/// Advances the completed graphics fence so fence-safe eviction & deferred destruction stay exact.
pub(crate) fn on_graphics_fence_complete(completed: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut st) = STATE.try_lock() {
        st.coherency.completed_deps.graphics =
            st.coherency.completed_deps.graphics.max(completed);
    }
}

/// Records a live CPU->GPU dirty upload range from `host.rs`.
pub(crate) fn note_cpu_upload(guest_addr: u64, len: u64) {
    if !enabled() || len == 0 {
        return;
    }
    CPU_WRITE_EVENTS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut st) = STATE.try_lock() {
        let _ = st.coherency.on_cpu_write(guest_addr, len);
    }
}

/// Records a live declared UAV GPU write range from `host.rs`.
pub(crate) fn note_gpu_uav_write(guest_addr: u64, len: u64, fence: u64) {
    if !enabled() || len == 0 {
        return;
    }
    GPU_WRITE_EVENTS.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut st) = STATE.try_lock()
        && let Ok(ids) = st.coherency.page_table.overlapping(guest_addr, len)
    {
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
