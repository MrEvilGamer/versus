//! Resource registry and transactional CPU/GPU synchronization.

use std::collections::HashMap;

use super::{GUEST_PAGE_SIZE, GgvlError, GuestMemoryPageTable, PageMeta, PageState, QueueDeps, ResidencyPriority, SyncPlan, TransferKind, TransferTicket};

/// Per-resource metadata in the GGVL registry.
#[derive(Debug, Clone)]
pub struct GuestResource {
    pub id: u64,
    pub guest_addr: u64,
    pub size: u64,
    pub is_derived_texture: bool,
    pub priority: ResidencyPriority,
    pub resident_in_vram: bool,
    pub last_access_frame: u64,
    pub active_deps: QueueDeps,
    /// Per-4KiB-page ownership & generation table.
    pub pages: Vec<PageMeta>,
}

impl GuestResource {
    fn page_range(&self, addr: u64, len: u64) -> Result<(usize, usize), GgvlError> {
        let end = addr.checked_add(len).ok_or(GgvlError::AddressOverflow)?;
        let res_end = self.guest_addr.checked_add(self.size).ok_or(GgvlError::AddressOverflow)?;
        if len == 0 || addr < self.guest_addr || end > res_end {
            return Err(GgvlError::OutOfBounds {
                addr,
                len,
                res_start: self.guest_addr,
                res_end,
            });
        }
        let first = ((addr - self.guest_addr) / GUEST_PAGE_SIZE) as usize;
        let last = ((end - 1 - self.guest_addr) / GUEST_PAGE_SIZE) as usize;
        Ok((first, last))
    }
}

/// Event-driven, range-level, transactional coherency manager.
#[derive(Default)]
pub struct ResourceCoherencyManager {
    next_id: u64,
    next_ticket: u64,
    pub page_table: GuestMemoryPageTable,
    pub resources: HashMap<u64, GuestResource>,
    pub tickets: HashMap<u64, TransferTicket>,
    pub completed_deps: QueueDeps,
    pub skipped_hash_passes: u64,
}

impl ResourceCoherencyManager {
    pub fn register(
        &mut self,
        guest_addr: u64,
        size: u64,
        is_derived_texture: bool,
        priority: ResidencyPriority,
    ) -> Result<u64, GgvlError> {
        let _end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        self.next_id += 1;
        let id = self.next_id;
        self.page_table.register_range(id, guest_addr, size)?;
        let num_pages = if size == 0 {
            0
        } else {
            size.div_ceil(GUEST_PAGE_SIZE) as usize
        };
        self.resources.insert(
            id,
            GuestResource {
                id,
                guest_addr,
                size,
                is_derived_texture,
                priority,
                resident_in_vram: true,
                last_access_frame: 0,
                active_deps: QueueDeps::default(),
                pages: vec![PageMeta::default(); num_pages],
            },
        );
        Ok(id)
    }

    pub fn unregister(&mut self, id: u64) -> Result<(), GgvlError> {
        let res = self.resources.get(&id).ok_or(GgvlError::UnknownResource(id))?;
        if !res.active_deps.is_satisfied_by(self.completed_deps)
            || self.tickets.values().any(|ticket| ticket.resource_id == id)
        {
            return Err(GgvlError::InFlight(id));
        }
        let (addr, size) = (res.guest_addr, res.size);
        self.page_table.unregister_range(id, addr, size);
        self.resources.remove(&id);
        Ok(())
    }

    /// Records a CPU write to `[guest_addr, guest_addr + size)` on exact intersecting pages only.
    /// Untouched GPU-authoritative pages in the same resource remain GPU-authoritative.
    pub fn on_cpu_write(&mut self, guest_addr: u64, size: u64) -> Result<usize, GgvlError> {
        let write_end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        let affected = self.page_table.overlapping(guest_addr, size)?;
        let mut pages_marked = 0usize;
        for id in affected {
            if let Some(res) = self.resources.get_mut(&id) {
                let res_end = res.guest_addr + res.size;
                let s = guest_addr.max(res.guest_addr);
                let e = write_end.min(res_end);
                if s >= e {
                    continue;
                }
                let first = ((s - res.guest_addr) / GUEST_PAGE_SIZE) as usize;
                let last = ((e - 1 - res.guest_addr) / GUEST_PAGE_SIZE) as usize;
                for pg in &mut res.pages[first..=last] {
                    pg.cpu_gen += 1;
                    pg.state = PageState::CpuAuthoritative;
                    pages_marked += 1;
                }
            }
        }
        Ok(pages_marked)
    }

    /// Records a GPU write to `[addr, addr + len)` on `id` with queue dependencies `deps`.
    /// Marks exact intersecting pages on `id` as `GpuAuthoritative` and overlapping aliases as
    /// `DerivedConversionPending` (textures) or `GpuAuthoritative` (buffers) without clobbering
    /// non-overlapping pages.
    pub fn on_gpu_write(
        &mut self,
        id: u64,
        addr: u64,
        len: u64,
        deps: QueueDeps,
    ) -> Result<(), GgvlError> {
        let res = self.resources.get(&id).ok_or(GgvlError::UnknownResource(id))?;
        let _ = res.page_range(addr, len)?;
        let write_end = addr + len;
        let aliases = self.page_table.overlapping(addr, len)?;
        for other_id in aliases {
            if let Some(other) = self.resources.get_mut(&other_id) {
                let other_end = other.guest_addr + other.size;
                let s = addr.max(other.guest_addr);
                let e = write_end.min(other_end);
                if s >= e {
                    continue;
                }
                other.active_deps.merge(deps);
                let first = ((s - other.guest_addr) / GUEST_PAGE_SIZE) as usize;
                let last = ((e - 1 - other.guest_addr) / GUEST_PAGE_SIZE) as usize;
                for pg in &mut other.pages[first..=last] {
                    pg.gpu_gen += 1;
                    pg.last_gpu_deps.merge(deps);
                    pg.state = if other_id == id {
                        PageState::GpuAuthoritative
                    } else if other.is_derived_texture {
                        PageState::DerivedConversionPending { deps }
                    } else {
                        PageState::GpuAuthoritative
                    };
                }
            }
        }
        Ok(())
    }

    /// Stage 1 (Plan): Plans lazy synchronization before a GPU read of `[addr, addr + len)`.
    /// Does NOT clear dirty state or mark pages coherent until `complete_transfer` is called.
    pub fn plan_gpu_read(
        &mut self,
        id: u64,
        addr: u64,
        len: u64,
        frame: u64,
    ) -> Result<SyncPlan, GgvlError> {
        let res = self.resources.get_mut(&id).ok_or(GgvlError::UnknownResource(id))?;
        if !res.resident_in_vram {
            return Err(GgvlError::NonResident(id));
        }
        let (first, last) = res.page_range(addr, len)?;
        if self.tickets.values().any(|ticket| {
            ticket.resource_id == id && ticket.pages.iter().any(|&(p, _, _)| (first..=last).contains(&p))
        }) {
            return Err(GgvlError::InFlight(id));
        }
        res.last_access_frame = frame;

        let mut upload_pages = Vec::new();
        let mut derived_pages = Vec::new();
        let mut wait_deps = QueueDeps::default();
        let mut has_gpu_auth = false;

        for (idx, pg) in res.pages[first..=last].iter().enumerate() {
            let p = first + idx;
            match pg.state {
                PageState::CpuAuthoritative => {
                    upload_pages.push((p, pg.cpu_gen, pg.gpu_gen));
                }
                PageState::DerivedConversionPending { deps } => {
                    wait_deps.merge(deps);
                    derived_pages.push((p, pg.cpu_gen, pg.gpu_gen));
                }
                PageState::GpuAuthoritative => {
                    has_gpu_auth = true;
                    wait_deps.merge(pg.last_gpu_deps);
                }
                PageState::Coherent
                | PageState::Uninitialized
                | PageState::CpuToGpuPending { .. }
                | PageState::GpuToCpuPending { .. } => {}
            }
        }

        if !upload_pages.is_empty() {
            let mut ranges: Vec<(u64, u64)> = Vec::new();
            for &(p, _, _) in &upload_pages {
                let s = res.guest_addr + (p as u64) * GUEST_PAGE_SIZE;
                let e = s.saturating_add(GUEST_PAGE_SIZE).min(res.guest_addr + res.size);
                if let Some((_, last_e)) = ranges.last_mut()
                    && *last_e == s
                {
                    *last_e = e;
                } else {
                    ranges.push((s, e));
                }
            }
            self.next_ticket += 1;
            let ticket_id = self.next_ticket;
            self.tickets.insert(
                ticket_id,
                TransferTicket {
                    ticket_id,
                    resource_id: id,
                    kind: TransferKind::CpuToGpu,
                    pages: upload_pages,
                    submitted_deps: None,
                },
            );
            return Ok(SyncPlan::UploadRanges { ticket_id, ranges });
        }

        if !derived_pages.is_empty() {
            self.next_ticket += 1;
            let ticket_id = self.next_ticket;
            self.tickets.insert(
                ticket_id,
                TransferTicket {
                    ticket_id,
                    resource_id: id,
                    kind: TransferKind::DerivedConvert,
                    pages: derived_pages,
                    submitted_deps: None,
                },
            );
            return Ok(SyncPlan::ConvertDerived { ticket_id, wait_deps });
        }

        self.skipped_hash_passes += 1;
        if has_gpu_auth {
            Ok(SyncPlan::GpuBarrierOnly { deps: wait_deps })
        } else {
            Ok(SyncPlan::NoOp)
        }
    }

    /// Stage 1 (Plan): Plans lazy synchronization before a CPU read of `[addr, addr + len)`.
    pub fn plan_cpu_read(&mut self, id: u64, addr: u64, len: u64) -> Result<SyncPlan, GgvlError> {
        let res = self.resources.get(&id).ok_or(GgvlError::UnknownResource(id))?;
        let (first, last) = res.page_range(addr, len)?;

        if self.tickets.values().any(|ticket| {
            ticket.resource_id == id && ticket.pages.iter().any(|&(p, _, _)| (first..=last).contains(&p))
        }) {
            return Err(GgvlError::InFlight(id));
        }

        let mut download_pages = Vec::new();
        let mut wait_deps = QueueDeps::default();

        for (idx, pg) in res.pages[first..=last].iter().enumerate() {
            let p = first + idx;
            match pg.state {
                PageState::GpuAuthoritative | PageState::DerivedConversionPending { .. } => {
                    wait_deps.merge(pg.last_gpu_deps);
                    download_pages.push((p, pg.cpu_gen, pg.gpu_gen));
                }
                _ => {}
            }
        }

        if download_pages.is_empty() {
            return Ok(SyncPlan::NoOp);
        }

        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for &(p, _, _) in &download_pages {
            let s = res.guest_addr + (p as u64) * GUEST_PAGE_SIZE;
            let e = s.saturating_add(GUEST_PAGE_SIZE).min(res.guest_addr + res.size);
            if let Some((_, last_e)) = ranges.last_mut()
                && *last_e == s
            {
                *last_e = e;
            } else {
                ranges.push((s, e));
            }
        }

        self.next_ticket += 1;
        let ticket_id = self.next_ticket;
        self.tickets.insert(
            ticket_id,
            TransferTicket {
                ticket_id,
                resource_id: id,
                kind: TransferKind::GpuToCpu,
                pages: download_pages,
                submitted_deps: None,
            },
        );
        Ok(SyncPlan::DownloadRanges {
            ticket_id,
            ranges,
            wait_deps,
        })
    }

    /// Stage 2 (Submit): Marks a planned transfer as submitted to GPU queues.
    pub fn submit_transfer(&mut self, ticket_id: u64, deps: QueueDeps) -> Result<(), GgvlError> {
        let ticket = self
            .tickets
            .get_mut(&ticket_id)
            .ok_or(GgvlError::UnknownTicket(ticket_id))?;
        if ticket.submitted_deps.is_some() {
            return Err(GgvlError::InFlight(ticket.resource_id));
        }
        let res = self.resources.get_mut(&ticket.resource_id)
            .ok_or(GgvlError::UnknownResource(ticket.resource_id))?;
        let mut deps = deps;
        for &(p, _, _) in &ticket.pages {
            if let Some(pg) = res.pages.get(p) {
                deps.merge(pg.last_gpu_deps);
            }
        }
        ticket.submitted_deps = Some(deps);
        {
            res.active_deps.merge(deps);
            for &(p, exp_cpu, exp_gpu) in &ticket.pages {
                if let Some(pg) = res.pages.get_mut(p)
                    && pg.cpu_gen == exp_cpu
                    && pg.gpu_gen == exp_gpu
                {
                    pg.state = match ticket.kind {
                        TransferKind::CpuToGpu => PageState::CpuToGpuPending { ticket_id },
                        TransferKind::GpuToCpu => PageState::GpuToCpuPending { ticket_id },
                        TransferKind::DerivedConvert => PageState::DerivedConversionPending { deps },
                    };
                }
            }
        }
        Ok(())
    }

    /// Stage 3 (Complete): Marks pages `Coherent` ONLY if their generation still matches the
    /// snapshot captured at planning time (prevents losing concurrent CPU/GPU writes).
    pub fn complete_transfer(&mut self, ticket_id: u64) -> Result<usize, GgvlError> {
        // Keep the ticket intact on premature completion so callers can retry.
        let ticket = self.tickets.get(&ticket_id).ok_or(GgvlError::UnknownTicket(ticket_id))?;
        if !ticket.submitted_deps.is_some_and(|deps| deps.is_satisfied_by(self.completed_deps)) {
            return Err(GgvlError::InFlight(ticket.resource_id));
        }
        if !self.resources.contains_key(&ticket.resource_id) {
            return Err(GgvlError::UnknownResource(ticket.resource_id));
        }
        let ticket = self
            .tickets
            .remove(&ticket_id)
            .ok_or(GgvlError::UnknownTicket(ticket_id))?;
        let mut committed = 0usize;
        if let Some(res) = self.resources.get_mut(&ticket.resource_id) {
            for &(p, exp_cpu, exp_gpu) in &ticket.pages {
                if let Some(pg) = res.pages.get_mut(p)
                    && pg.cpu_gen == exp_cpu
                    && pg.gpu_gen == exp_gpu
                {
                    pg.state = PageState::Coherent;
                    committed += 1;
                }
            }
        }
        Ok(committed)
    }

    /// Cancels a planned or failed transfer without corrupting authoritative state.
    pub fn cancel_transfer(&mut self, ticket_id: u64) -> Result<(), GgvlError> {
        let ticket = self.tickets.get(&ticket_id).ok_or(GgvlError::UnknownTicket(ticket_id))?;
        if let Some(deps) = ticket.submitted_deps {
            if !deps.is_satisfied_by(self.completed_deps) {
                return Err(GgvlError::InFlight(ticket.resource_id));
            }
        } else {
            // Planning never changed ownership, so an unsubmitted cancellation
            // must not overwrite page state.
            self.tickets.remove(&ticket_id);
            return Ok(());
        }
        let ticket = self
            .tickets
            .remove(&ticket_id)
            .ok_or(GgvlError::UnknownTicket(ticket_id))?;
        if let Some(res) = self.resources.get_mut(&ticket.resource_id) {
            for &(p, exp_cpu, exp_gpu) in &ticket.pages {
                if let Some(pg) = res.pages.get_mut(p)
                    && pg.cpu_gen == exp_cpu
                    && pg.gpu_gen == exp_gpu
                {
                    pg.state = match ticket.kind {
                        TransferKind::CpuToGpu => PageState::CpuAuthoritative,
                        TransferKind::GpuToCpu => PageState::GpuAuthoritative,
                        TransferKind::DerivedConvert => PageState::DerivedConversionPending {
                            deps: pg.last_gpu_deps,
                        },
                    };
                }
            }
        }
        Ok(())
    }
}
