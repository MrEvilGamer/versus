//! `versus-backend`: NVIDIA-first runtime profile & next-gen GPU backend scaffolding.
//!
//! Applies proven 8 GB NVIDIA (Wave32) VRAM, copy-queue, descriptor-sync,
//! lookup-cache, and AVX2 defaults before the GPU layer reads its `OnceLock`
//! configuration, while keeping every switch overridable via environment variables.

/// Active GPU backend target for Versus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Tuned D3D12.X translation layer with NVIDIA Wave32 + 8 GB VRAM defaults.
    D3D12Tuned,
    /// Future Vulkan (`VK_KHR_buffer_device_address` + `VK_EXT_descriptor_buffer`) backend.
    VulkanBda,
}

/// Default environment tuning for 8 GB NVIDIA GPUs.
/// Every entry is only set when the user has not explicitly provided a value.
const NVIDIA_DEFAULTS: &[(&str, &str)] = &[
    ("VICTORY_NO_WATCHDOG", "1"),
    ("VICTORY_DISPLAY_MODE", "640x360"),
    ("VICTORY_LEVELS_ON_DEMAND", "1"),
    ("VICTORY_MAX_TEXTURE_DIM", "256"),
    ("VICTORY_VIDEO_BUDGET_MB", "5500"),
    ("VICTORY_TEXTURE_NO_WAIT", "1"),
    ("VICTORY_TEXTURE_BACK", "1"),
    ("VICTORY_TEXTURE_PAGES", "1"),
    ("VICTORY_INDIRECT_GATHER", "1"),
    ("VICTORY_HOIST_CLAMPS", "1"),
    ("VICTORY_CARD_BUFFERS_MIN_MB", "1"),
    ("VICTORY_CARD_BUFFERS_MB", "1024"),
    ("VICTORY_CARD_HASH_LEN", "1"),
    ("VICTORY_CARD_HASH_WIDE", "1"),
    ("VICTORY_CARD_REHASH_RUNS", "1"),
    ("VICTORY_QUEUE2", "1"),
    ("VICTORY_ASYNC_SUBMIT", "1"),
    ("VICTORY_ABSENT_MS", "told"),
    ("VICTORY_CHANGED_RUNS_AVX2", "1"),
    ("VICTORY_LOOKUP_CACHE", "1"),
    ("VICTORY_LIGHT_REFRESH", "1"),
    ("VICTORY_STEP_INDEX", "1"),
    ("VICTORY_FILLS_PLAIN", "1"),
    ("VICTORY_HEAP_SYNC_CUT", "1"),
    ("VICTORY_VIEWS_REMADE_QUIET", "1"),
    ("VICTORY_BVH_NO_HIT", "1"),
    // avatarmanz 25-FPS engine switches
    ("VICTORY_SEND_OWN", "1"),
    ("VICTORY_UP_QUEUE", "1"),
    ("VICTORY_RELAY", "2"),
    ("VICTORY_RELAY_COPY", "1"),
    ("VICTORY_JOIN_BARRIERS", "1"),
    ("VICTORY_SYNC_IN_PLACE", "1"),
    ("VICTORY_SPANS_QUICK", "1"),
    ("VICTORY_UNDER_FUSED", "1"),
    ("VICTORY_SHOW_NEW_LATER", "1"),
    ("VICTORY_METER_THREAD", "1"),
    ("VICTORY_LANE_CORE", "1"),
    ("VICTORY_WAIT_RESIDENT", "0"),
];

/// Applies Versus's NVIDIA-first defaults to the process environment.
///
/// Must be called on the main thread at startup before any worker threads are spawned.
pub fn apply_nvidia_defaults() -> BackendKind {
    let mut applied = 0usize;
    for &(key, val) in NVIDIA_DEFAULTS {
        if std::env::var_os(key).is_none() {
            // SAFETY: called at the very start of `main()` before spawning threads.
            unsafe {
                std::env::set_var(key, val);
            }
            applied += 1;
        }
    }
    tracing::info!(
        applied,
        backend = "d3d12-tuned",
        "versus: NVIDIA-first runtime defaults active"
    );
    BackendKind::D3D12Tuned
}
