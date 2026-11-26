//! Live bridge wiring hardened `versus_backend::ggvl` (Milestone 1.1) into `victory-gpu`.

use std::sync::{LazyLock, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use versus_backend::ggvl::{
    GgvlError, QueueDeps, ResidencyManager, ResidencyPriority, ResourceCoherencyManager, SyncPlan,
};

struct GgvlState {
    coherency: ResourceCoherencyManager,
    residency: ResidencyManager,
}

static STATE: LazyLock<Mutex<GgvlState>> = LazyLock::new(|| {
    Mutex::new(GgvlState {
        coherency: ResourceCoherencyManager::default(),
        residency: ResidencyManager::new(6 * 1024 * 1024 * 1024),
    })
});

static BUDGET_UPDATES: AtomicU64 = AtomicU64::new(0);
static LAST_EVICT_CANDIDATES: AtomicU64 = AtomicU64::new(0);

/// Whether GGVL tracking is active (`VICTORY_GGVL=0` disables it).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GGVL").is_none_or(|v| v != "0"))
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
                skipped_hashes = st.coherency.skipped_hash_passes,
                evict_candidates = evict.len(),
                "ggvl: live residency & coherency status"
            );
        }
    }
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
