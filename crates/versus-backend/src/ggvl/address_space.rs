//! Sparse guest-address to resource index.

use std::collections::{BTreeMap, HashSet};

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
        // Small queries avoid allocating a hash table. For heavily aliased
        // ranges, switch to hashed membership once the linear scan grows.
        // Keep the original page/registration traversal order in the result.
        const LINEAR_LIMIT: usize = 16;
        let mut out = Vec::new();
        let mut seen: Option<HashSet<u64>> = None;
        for (_, ids) in self.pages.range(first..=last) {
            for &id in ids {
                if let Some(seen) = seen.as_mut() {
                    if seen.insert(id) {
                        out.push(id);
                    }
                } else if !out.contains(&id) {
                    out.push(id);
                    if out.len() == LINEAR_LIMIT {
                        seen = Some(out.iter().copied().collect());
                    }
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_preserves_first_seen_order_with_many_aliases() {
        let mut table = GuestMemoryPageTable::default();
        for id in (1..=32).rev() {
            table.register_range(id, GUEST_PAGE_SIZE, GUEST_PAGE_SIZE * 3).unwrap();
        }
        table.register_range(100, GUEST_PAGE_SIZE * 2, GUEST_PAGE_SIZE).unwrap();
        table.register_range(32, GUEST_PAGE_SIZE, GUEST_PAGE_SIZE).unwrap();
        let mut expected: Vec<u64> = (1..=32).rev().collect();
        expected.push(100);
        assert_eq!(table.overlapping(GUEST_PAGE_SIZE, GUEST_PAGE_SIZE * 3).unwrap(), expected);

        table.unregister_range(16, GUEST_PAGE_SIZE, GUEST_PAGE_SIZE * 3);
        expected.retain(|&id| id != 16);
        assert_eq!(table.overlapping(GUEST_PAGE_SIZE, GUEST_PAGE_SIZE * 3).unwrap(), expected);
    }

    #[test]
    fn small_queries_keep_page_granular_semantics() {
        let mut table = GuestMemoryPageTable::default();
        table.register_range(3, 1, 2).unwrap();
        table.register_range(2, GUEST_PAGE_SIZE - 1, 2).unwrap();
        table.register_range(1, GUEST_PAGE_SIZE * 3, 1).unwrap();
        assert_eq!(table.overlapping(0, 1).unwrap(), vec![3, 2]);
        assert_eq!(table.overlapping(GUEST_PAGE_SIZE, 1).unwrap(), vec![2]);
        assert!(table.overlapping(GUEST_PAGE_SIZE * 2, 1).unwrap().is_empty());
        assert!(table.overlapping(0, 0).unwrap().is_empty());
        assert_eq!(table.overlapping(u64::MAX, 1), Err(GgvlError::AddressOverflow));
    }

    #[test]
    fn adaptive_lookup_matches_linear_reference() {
        let mut table = GuestMemoryPageTable::default();
        for id in 1..=96u64 {
            let start = ((id * 37) % 31) * GUEST_PAGE_SIZE + id % 11;
            let len = ((id * 13) % 9 + 1) * GUEST_PAGE_SIZE;
            table.register_range(id, start, len).unwrap();
            if id % 7 == 0 {
                table.unregister_range(id, start, len);
            }
        }
        for first in 0..40u64 {
            for count in 1..=12u64 {
                let mut expected = Vec::new();
                for (_, ids) in table.pages.range(first..=first + count - 1) {
                    for &id in ids {
                        if !expected.contains(&id) {
                            expected.push(id);
                        }
                    }
                }
                assert_eq!(
                    table.overlapping(first * GUEST_PAGE_SIZE, count * GUEST_PAGE_SIZE).unwrap(),
                    expected
                );
            }
        }
    }
}
