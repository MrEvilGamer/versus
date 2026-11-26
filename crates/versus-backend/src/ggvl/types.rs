//! Shared GGVL memory ownership, fence and transfer contracts.

/// Logical guest tracking page size (4 KiB).
pub const GUEST_PAGE_SIZE: u64 = 4096;
/// Hardware D3D12 reserved-resource tile size (64 KiB).
pub const D3D12_TILE_SIZE: u64 = 65_536;

/// Explicit GGVL error type (no silent drops on invalid/uninitialized/overflowed accesses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GgvlError {
    UnknownResource(u64),
    AddressOverflow,
    OutOfBounds { addr: u64, len: u64, res_start: u64, res_end: u64 },
    NonResident(u64),
    InFlight(u64),
    UnknownTicket(u64),
}

/// Queue-aware GPU fence dependency vector.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueDeps {
    pub graphics: u64,
    pub compute: u64,
    pub copy: u64,
}

impl QueueDeps {
    pub fn merge(&mut self, other: QueueDeps) {
        self.graphics = self.graphics.max(other.graphics);
        self.compute = self.compute.max(other.compute);
        self.copy = self.copy.max(other.copy);
    }

    pub fn is_satisfied_by(&self, completed: QueueDeps) -> bool {
        self.graphics <= completed.graphics
            && self.compute <= completed.compute
            && self.copy <= completed.copy
    }
}

/// Per-4KiB-page authoritative ownership state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    Uninitialized,
    CpuAuthoritative,
    GpuAuthoritative,
    Coherent,
    CpuToGpuPending { ticket_id: u64 },
    GpuToCpuPending { ticket_id: u64 },
    DerivedConversionPending { deps: QueueDeps },
}

/// Per-4KiB-page metadata inside a tracked resource.
#[derive(Debug, Clone, Copy)]
pub struct PageMeta {
    pub state: PageState,
    pub cpu_gen: u64,
    pub gpu_gen: u64,
    pub last_gpu_deps: QueueDeps,
}

impl Default for PageMeta {
    fn default() -> Self {
        Self {
            state: PageState::Uninitialized,
            cpu_gen: 0,
            gpu_gen: 0,
            last_gpu_deps: QueueDeps::default(),
        }
    }
}

/// Kind of transactional transfer ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    CpuToGpu,
    GpuToCpu,
    DerivedConvert,
}

/// Generation-checked transfer ticket separating Plan -> Submit -> Complete/Cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferTicket {
    pub ticket_id: u64,
    pub resource_id: u64,
    pub kind: TransferKind,
    /// `(page_index, expected_cpu_gen, expected_gpu_gen)` captured at planning time.
    pub pages: Vec<(usize, u64, u64)>,
    pub submitted_deps: Option<QueueDeps>,
}

/// Synchronization plan returned by `plan_gpu_read` / `plan_cpu_read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncPlan {
    /// No hash, compare, upload, or readback required.
    NoOp,
    /// Upload dirty guest byte ranges; caller must `submit_transfer` then `complete_transfer`.
    UploadRanges {
        ticket_id: u64,
        ranges: Vec<(u64, u64)>,
    },
    /// GPU-to-GPU barrier/fence only — zero CPU involvement.
    GpuBarrierOnly { deps: QueueDeps },
    /// Download GPU-authoritative byte ranges; caller must `submit_transfer` then `complete_transfer`.
    DownloadRanges {
        ticket_id: u64,
        ranges: Vec<(u64, u64)>,
        wait_deps: QueueDeps,
    },
    /// Re-derive aliased texture/RT representation on GPU.
    ConvertDerived {
        ticket_id: u64,
        wait_deps: QueueDeps,
    },
}
