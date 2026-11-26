//! Backend-independent shader wave classification contract.

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
