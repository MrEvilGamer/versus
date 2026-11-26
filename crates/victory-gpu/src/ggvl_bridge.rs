//! Live bridge wiring `versus_backend::ggvl` into `victory-gpu`'s D3D12 runtime.
//!
//! Tracks guest address-space registrations, CPU/GPU write events, fence
//! completions, and DXGI `QueryVideoMemoryInfo` residency budgets.

use std::sync::{LazyLock, Mutex, OnceLock};
use std::sync::atomic::{AtomicU64, Ordering};

use versus_backend::ggvl::{
    ResidencyManager, ResidencyPriority, ResourceCoherencyManager, SyncAction,
};

struct GgvlState {
    coherency: ResourceCoherencyManager,
    residency: ResidencyManager,
}

static STATE: LazyLock<Mutex<GgvlState>> = LazyLock::new(|| {
    Mutex::new(GgvlState {
        coherency: ResourceCoherencyManager::default(),
        // Default 6 GiB until first `QueryVideoMemoryInfo` updates the live DXGI budget.
        residency: ResidencyManager::new(6 * 1024 * 1024 * 1024),
    })
});

static BUDGET_UPDATES: AtomicU64 = AtomicU64::new(0);
static LAST_EVICT_CANDIDATES: AtomicU64 = AtomicU64::new(0);
static LAST_BUDGET_MB: AtomicU64 = AtomicU64::new(0);
static LAST_USAGE_MB: AtomicU64 = AtomicU64::new(0);

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
    LAST_BUDGET_MB.store(budget_bytes / (1024 * 1024), Ordering::Relaxed);
    LAST_USAGE_MB.store(usage_bytes / (1024 * 1024), Ordering::Relaxed);
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

/// Registers a guest buffer or derived texture in the GGVL address space.
#[allow(dead_code)]
pub(crate) fn register(guest_addr: u64, size: u64, is_derived_texture: bool, priority: ResidencyPriority) -> u64 {
    if !enabled() {
        return 0;
    }
    STATE
        .lock()
        .map(|mut st| st.coherency.register(guest_addr, size, is_derived_texture, priority))
        .unwrap_or(0)
}

/// Records a CPU write range and invalidates overlapping aliases.
#[allow(dead_code)]
pub(crate) fn on_cpu_write(guest_addr: u64, size: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut st) = STATE.lock() {
        st.coherency.on_cpu_write(guest_addr, size);
    }
}

/// Records a GPU write associated with completion `fence`.
#[allow(dead_code)]
pub(crate) fn on_gpu_write(id: u64, fence: u64) {
    if !enabled() || id == 0 {
        return;
    }
    if let Ok(mut st) = STATE.lock() {
        st.coherency.on_gpu_write(id, fence);
    }
}

/// Plans lazy synchronization before a GPU read of `id`.
#[allow(dead_code)]
pub(crate) fn prepare_gpu_read(id: u64, frame: u64) -> SyncAction {
    if !enabled() || id == 0 {
        return SyncAction::NoOp;
    }
    STATE
        .lock()
        .map(|mut st| st.coherency.prepare_gpu_read(id, frame))
        .unwrap_or(SyncAction::NoOp)
}

/// Advances the completed GPU fence so deferred destructions and evictions can proceed.
#[allow(dead_code)]
pub(crate) fn on_fence_complete(fence: u64) {
    if !enabled() {
        return;
    }
    if let Ok(mut st) = STATE.try_lock() {
        st.coherency.completed_fence = st.coherency.completed_fence.max(fence);
    }
}
