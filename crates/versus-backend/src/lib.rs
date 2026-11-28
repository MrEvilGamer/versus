//! `versus-backend`: Adaptive hardware-tier runtime profiles & GGVL core.
//!
//! Automatically detects host physical RAM and CPU core count at startup so
//! 16 GB / 6-core systems (e.g. Ryzen 5 5600X + RTX 4060 8 GB) get safe,
//! low-RAM, non-starving defaults while 32 GB+ / high-core systems (e.g.
//! Ryzen 9 7900X + RTX 4060 Ti 8 GB) get high-throughput defaults.
//! Every switch remains overridable via environment variables.

pub mod ggvl;

/// Active GPU backend target for Versus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    D3D12Tuned,
    VulkanBda,
}

/// Detected host hardware profile tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareTier {
    /// <= 20 GB physical RAM or <= 12 logical CPU threads (e.g. 16 GB DDR4 + 6c/12t).
    Constrained16GB,
    /// > 20 GB physical RAM and > 12 logical CPU threads (e.g. 32 GB DDR5 + 12c/24t).
    HighEnd32GB,
}

/// Pure tier classifier for unit testing and runtime detection.
pub fn classify_hardware_tier(ram_gb: u64, logical_cpus: usize) -> HardwareTier {
    if ram_gb <= 20 || logical_cpus <= 12 {
        HardwareTier::Constrained16GB
    } else {
        HardwareTier::HighEnd32GB
    }
}

fn detect_ram_gb() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            return status.ullTotalPhys / (1024 * 1024 * 1024);
        }
    }
    16
}

/// Safe shared defaults for all NVIDIA GPUs (match upstream avatarmanz safe paths).
const SHARED_DEFAULTS: &[(&str, &str)] = &[
    ("VICTORY_NO_WATCHDOG", "1"),
    // Off: the known-good texture path (launch-versus-648.bat). On an RTX
    // 4060 Ti 8 GB with levels on demand and the texture pool on, 393 to
    // 5,003 textures waited for memory in the world and the pool's blocks
    // kept 1-1.5 GB of gaps; with both off 14 waited and the card stayed
    // under its budget (texlogs/nopool, 2026-11-28).
    ("VICTORY_LEVELS_ON_DEMAND", "0"),
    ("VICTORY_TEXTURE_POOL", "0"),
    // The least indirect light with the ambient stand-in on (VICTORY_AMBIENT):
    // without it unlit interiors are black where the title's traced light
    // would be (the prison cell, texlogs/least, 2026-11-28).
    ("VICTORY_AMBIENT_LEAST", "0.12"),
    ("VICTORY_VIDEO_BUDGET_MB", "5500"),
    // Off: a refresh that went on while the title remapped its memory filled
    // textures with what lay there (the prison walls blotched, 2026-11-28).
    ("VICTORY_TEXTURE_NO_WAIT", "0"),
    ("VICTORY_TEXTURE_BACK", "1"),
    ("VICTORY_TEXTURE_PAGES", "1"),
    ("VICTORY_INDIRECT_GATHER", "1"),
    ("VICTORY_HOIST_CLAMPS", "1"),
    ("VICTORY_CARD_BUFFERS_MIN_MB", "1"),
    ("VICTORY_CARD_HASH_LEN", "1"),
    ("VICTORY_CARD_HASH_WIDE", "1"),
    ("VICTORY_CARD_REHASH_RUNS", "1"),
    ("VICTORY_ASYNC_SUBMIT", "1"),
    ("VICTORY_ABSENT_MS", "told"),
    ("VICTORY_RELAY", "2"),
    ("VICTORY_RELAY_COPY", "1"),
    ("VICTORY_SEND_OWN", "1"),
    ("VICTORY_SYNC_IN_PLACE", "1"),
    ("VICTORY_SPANS_QUICK", "1"),
    ("VICTORY_UNDER_FUSED", "1"),
    ("VICTORY_SHOW_NEW_LATER", "1"),
    ("VICTORY_METER_THREAD", "1"),
    ("VICTORY_WAIT_RESIDENT", "0"),
    ("VICTORY_GGVL_SKIP_READONLY_HASH", "1"),
    ("VICTORY_CARD_STEPS", "2"),
    // Neutral graphics quality defaults (independently overridable via env vars).
    ("VICTORY_DISPLAY_MODE", "960x540"),
    ("VICTORY_MAX_TEXTURE_DIM", "512"),
];

/// Conservative memory/CPU budgets for 16 GB RAM / 6-core CPUs (no core pinning, tight RAM cap).
const TIER_16GB_DEFAULTS: &[(&str, &str)] = &[
    ("VICTORY_CARD_BUFFERS_MB", "512"),
    ("VICTORY_WINDOW_BUDGET_MB", "512"),
];

/// Higher memory/CPU budgets for 32 GB+ RAM / >12-thread CPUs.
const TIER_32GB_DEFAULTS: &[(&str, &str)] = &[
    ("VICTORY_CARD_BUFFERS_MB", "1024"),
    ("VICTORY_WINDOW_BUDGET_MB", "2048"),
    ("VICTORY_LANE_CORE", "1"),
];

/// Applies adaptive hardware-tier defaults to the process environment.
///
/// Must be called on the main thread at startup before spawning worker threads.
pub fn apply_nvidia_defaults() -> BackendKind {
    let ram_gb = detect_ram_gb();
    let logical_cpus = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(8);
    let tier = classify_hardware_tier(ram_gb, logical_cpus);
    let tier_defaults = match tier {
        HardwareTier::Constrained16GB => TIER_16GB_DEFAULTS,
        HardwareTier::HighEnd32GB => TIER_32GB_DEFAULTS,
    };

    let mut applied = 0usize;
    for &(key, val) in SHARED_DEFAULTS.iter().chain(tier_defaults.iter()) {
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
        ram_gb,
        logical_cpus,
        ?tier,
        backend = "d3d12-tuned",
        "versus: adaptive hardware profile active"
    );
    BackendKind::D3D12Tuned
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn classifies_16gb_6core_as_constrained() {
        assert_eq!(classify_hardware_tier(16, 12), HardwareTier::Constrained16GB);
        assert_eq!(classify_hardware_tier(32, 12), HardwareTier::Constrained16GB);
        assert_eq!(classify_hardware_tier(16, 24), HardwareTier::Constrained16GB);
    }

    #[test]
    fn classifies_32gb_12core_as_high_end() {
        assert_eq!(classify_hardware_tier(32, 24), HardwareTier::HighEnd32GB);
        assert_eq!(classify_hardware_tier(64, 16), HardwareTier::HighEnd32GB);
    }
}
