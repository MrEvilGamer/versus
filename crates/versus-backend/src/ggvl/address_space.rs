//! Sparse guest-address to resource index.

use std::collections::BTreeMap;

use super::{GUEST_PAGE_SIZE, GgvlError};

/// Sparse page table mapping 4 KiB guest pages to active resource IDs.
#[derive(Default)]
pub struct GuestMemoryPageTable {
    pages: BTreeMap<u64, Vec<u64>>,
}

impl GuestMemoryPageTable {
    pub fn register_range(&mut self, id: u64, guest_addr: u64, size: u64) -> Result<(), GgvlError> {
        let end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        if size == 0 {
            return Ok(());
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (end - 1) / GUEST_PAGE_SIZE;
        for p in first..=last {
            let entry = self.pages.entry(p).or_default();
            if !entry.contains(&id) {
                entry.push(id);
            }
        }
        Ok(())
    }

    pub fn unregister_range(&mut self, id: u64, guest_addr: u64, size: u64) {
        let Some(end) = guest_addr.checked_add(size) else {
            return;
        };
        if size == 0 {
            return;
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (end - 1) / GUEST_PAGE_SIZE;
        for p in first..=last {
            if let Some(entry) = self.pages.get_mut(&p) {
                entry.retain(|&x| x != id);
                if entry.is_empty() {
                    self.pages.remove(&p);
                }
            }
        }
    }

    pub fn overlapping(&self, guest_addr: u64, size: u64) -> Result<Vec<u64>, GgvlError> {
        let end = guest_addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let first = guest_addr / GUEST_PAGE_SIZE;
        let last = (end - 1) / GUEST_PAGE_SIZE;
        let mut out = Vec::new();
        for (_, ids) in self.pages.range(first..=last) {
            for &id in ids {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }
}
