//! VRAM budget policy and fence-aware candidate selection.

use super::{GuestResource, ResourceCoherencyManager};

/// Residency priority tier for VRAM budget management.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResidencyPriority {
    Low = 0,
    Medium = 1,
    High = 2,
    Critical = 3,
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
