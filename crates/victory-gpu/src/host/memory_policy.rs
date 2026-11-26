//! Host memory pressure sampling and cache retention policy.
//!
//! Keep this independent of GPU submission and resource lifecycle logic.

use std::sync::OnceLock;

/// Available physical RAM on the host in bytes, from `GlobalMemoryStatusEx`.
pub(crate) fn available_physical_ram() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullAvailPhys)
}

/// Whether memory the card was given stays with it while there is room (on
/// by default; `VICTORY_KEEP_FREED=0`: it is given back as before). A shader
/// that reads past a view (a raw load through a typed, structured or
/// constant buffer view has no end on AMD: `indirect::tests::raw_loads_past_views`)
/// or through a slot that shows wrong data reads whatever lies there, and
/// nothing happens; where memory was given back it is a page fault, and the
/// device is lost. Both losses of 2026-11-25 were reads in memory given back
/// seconds before (a block of textures; a list's scratch and a window of
/// title memory). Kept are: empty blocks of textures ([`crate::pool`]) below
/// 85 % of the card's budget, windows of title memory and staging buffers
/// for five minutes with 12 GiB of the PC's memory free, and 1 GiB of
/// scratch chunks.
pub(crate) fn keep_freed() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KEEP_FREED").is_none_or(|v| v != "0"))
}

/// Effective idle timeout (in milliseconds) for system-RAM caches (staging
/// buffers, loose shadows, and the mapped pipeline library), scaled down
/// when free physical RAM is low. Never returns less than 2,000 ms.
/// With plenty of it five minutes ([`keep_freed`]).
pub(crate) fn ram_idle_ms(avail_phys: Option<u64>) -> u64 {
    ram_idle_for(avail_phys, keep_freed())
}

fn ram_idle_for(avail_phys: Option<u64>, keep: bool) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    const KEPT_MS: u64 = 300_000;
    if keep && avail_phys.is_some_and(|avail| avail >= 12 * GIB) {
        return KEPT_MS;
    }
    const NORMAL_MS: u64 = 10_000;
    const MODERATE_MS: u64 = 5_000;
    const HIGH_MS: u64 = 2_000;
    let Some(avail) = avail_phys else {
        return NORMAL_MS;
    };
    if avail < 2 * GIB {
        HIGH_MS
    } else if avail < 4 * GIB {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

/// System-RAM pressure band for optional caches. Resident working set and
/// private bytes describe this process; free physical and commit headroom
/// guard the PC and other processes. Windows API queries never change the
/// pagefile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RamBand {
    Calm,
    Watch,
    High,
}

/// Band for one sample. Entering Watch or High needs a higher watermark than
/// leaving it, so a value near a threshold does not toggle the policy every
/// sample.
pub(crate) fn ram_band(
    previous: RamBand,
    working: u64,
    private: u64,
    free_physical: u64,
    commit_headroom: u64,
    total_physical: u64,
    commit_limit: u64,
) -> RamBand {
    const GIB: u64 = 1024 * 1024 * 1024;
    // Fractions of the machine, so a 64 GB host is not treated like the
    // 32 GB one. The free and headroom floors stay absolute: pressure is
    // pressure however big the machine is.
    let high = private * 5 >= commit_limit * 4
        || working * 2 >= total_physical
        || free_physical < 2 * GIB
        || commit_headroom < GIB;
    let watch = private * 10 >= commit_limit * 7
        || working * 4 >= total_physical
        || free_physical < 4 * GIB
        || commit_headroom < 2 * GIB;
    let wanted = if high {
        RamBand::High
    } else if watch {
        RamBand::Watch
    } else {
        RamBand::Calm
    };
    let safe_high = private * 20 <= commit_limit * 13
        && working * 4 <= total_physical
        && free_physical >= 4 * GIB
        && commit_headroom >= 3 * GIB;
    let safe_watch = private * 5 <= commit_limit * 3
        && working * 5 <= total_physical
        && free_physical >= 6 * GIB
        && commit_headroom >= 3 * GIB;
    match (previous, wanted) {
        (RamBand::High, RamBand::Calm) if !safe_high => RamBand::Watch,
        (RamBand::Watch, RamBand::Calm) if !safe_watch => RamBand::Watch,
        _ => wanted,
    }
}

/// The six numbers [`ram_band`] needs, from Windows. `None` if a query fails,
/// so a caller can keep its old band instead of guessing.
#[cfg(windows)]
pub(crate) fn ram_sample() -> Option<(u64, u64, u64, u64, u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: plain data the call fills in.
    if unsafe { GlobalMemoryStatusEx(&mut m) } == 0 {
        return None;
    }
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    if unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } == 0 {
        return None;
    }
    Some((
        c.WorkingSetSize as u64,
        c.PagefileUsage as u64,
        m.ullAvailPhys,
        m.ullAvailPageFile,
        m.ullTotalPhys,
        m.ullTotalPageFile,
    ))
}

#[cfg(test)]
mod tests {
    use super::{RamBand, ram_band};

    #[test]
    fn ram_band_tracks_resident_and_commit_pressure() {
        const M: u64 = 1 << 20;
        // The measured local Series S baseline stayed calm (32 GB machine,
        // roughly 11 GB commit limit).
        assert_eq!(
            ram_band(RamBand::Calm, 3857 * M, 7500 * M, 15900 * M, 3962 * M, 32 * 1024 * M, 11 * 1024 * M),
            RamBand::Calm
        );
        // Private commit alone crosses the watch watermark.
        assert_eq!(
            ram_band(RamBand::Calm, 3857 * M, 8200 * M, 15900 * M, 3962 * M, 32 * 1024 * M, 11 * 1024 * M),
            RamBand::Watch
        );
        // The measured Series X commit failure was High.
        assert_eq!(
            ram_band(RamBand::Calm, 4452 * M, 10191 * M, 16047 * M, 154 * M, 32 * 1024 * M, 11 * 1024 * M),
            RamBand::High
        );
    }

    #[test]
    fn ram_band_exits_only_below_lower_watermarks() {
        const M: u64 = 1 << 20;
        assert_eq!(
            ram_band(RamBand::High, 4452 * M, 8900 * M, 16047 * M, 1200 * M, 32 * 1024 * M, 11 * 1024 * M),
            RamBand::Watch,
            "just under the entry mark does not drop straight to calm"
        );
        assert_eq!(
            ram_band(RamBand::Watch, 3857 * M, 6600 * M, 15900 * M, 3962 * M, 32 * 1024 * M, 11 * 1024 * M),
            RamBand::Calm,
            "all lower watermarks met"
        );
    }

    #[test]
    fn ram_band_does_not_toggle_below_the_entry_watermark() {
        const M: u64 = 1 << 20;
        let mut band = RamBand::Calm;
        for private in [7500 * M, 8200 * M, 8200 * M, 8200 * M] {
            band = ram_band(band, 3857 * M, private, 15900 * M, 3962 * M, 32 * 1024 * M, 11 * 1024 * M);
        }
        assert_eq!(band, RamBand::Watch);
        band = ram_band(band, 3857 * M, 7800 * M, 15900 * M, 3962 * M, 32 * 1024 * M, 11 * 1024 * M);
        assert_eq!(band, RamBand::Watch);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        let ram_idle_ms = |avail| super::ram_idle_for(avail, false);
        assert_eq!(super::ram_idle_for(Some(12 << 30), true), 300_000);
        assert_eq!(super::ram_idle_for(Some((12 << 30) - 1), true), 10_000);
        assert_eq!(super::ram_idle_for(None, true), 10_000);
        let gib = 1024 * 1024 * 1024;
        // Normal (>= 4 GiB)
        assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
        // Moderate (2 GiB..4 GiB)
        assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
        // High (< 2 GiB)
        assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(ram_idle_ms(Some(0)), 2_000);
        // Missing falls back to normal
        assert_eq!(ram_idle_ms(None), 10_000);
    }

}
