//! Sparse guest-address index with compressed, page-granular membership runs.

use std::collections::{BTreeMap, HashSet};
use std::ops::Bound::{Excluded, Included};

use super::{GUEST_PAGE_SIZE, GgvlError};

/// A boundary changes the ordered resource membership of subsequent 4 KiB
/// pages until the next boundary. Missing leading pages are empty. Adjacent
/// runs with identical membership are coalesced, and a trailing empty
/// boundary terminates the last populated run.
///
/// This avoids allocating one Vec and one tree entry per page of large
/// resources. Exact byte overlap remains the caller's responsibility.
#[derive(Default)]
pub struct GuestMemoryPageTable {
    boundaries: BTreeMap<u64, Vec<u64>>,
}

impl GuestMemoryPageTable {
    fn page_range(addr: u64, size: u64) -> Result<Option<(u64, u64)>, GgvlError> {
        let end = addr.checked_add(size).ok_or(GgvlError::AddressOverflow)?;
        Ok((size != 0).then(|| (addr / GUEST_PAGE_SIZE, (end - 1) / GUEST_PAGE_SIZE + 1)))
    }

    // Split a run without altering its membership.
    fn split_at(&mut self, page: u64) {
        if self.boundaries.contains_key(&page) {
            return;
        }
        let ids = self.boundaries.range(..page).next_back()
            .map(|(_, ids)| ids.clone()).unwrap_or_default();
        self.boundaries.insert(page, ids);
    }

    // Only boundaries inside the modified span and its right edge can
    // become redundant. The left predecessor remains unchanged.
    fn coalesce(&mut self, first: u64, end: u64) {
        let mut previous = self.boundaries.range(..first).next_back()
            .map(|(_, ids)| ids.as_slice()).unwrap_or(&[]);
        let mut redundant = Vec::new();
        for (&page, ids) in self.boundaries.range(first..=end) {
            if previous == ids.as_slice() {
                redundant.push(page);
            } else {
                previous = ids;
            }
        }
        for page in redundant {
            self.boundaries.remove(&page);
        }
    }

    pub fn register_range(&mut self, id: u64, guest_addr: u64, size: u64) -> Result<(), GgvlError> {
        let Some((first, end)) = Self::page_range(guest_addr, size)? else {
            return Ok(());
        };
        self.split_at(first);
        self.split_at(end);
        for (_, ids) in self.boundaries.range_mut(first..end) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        self.coalesce(first, end);
        Ok(())
    }

    pub fn unregister_range(&mut self, id: u64, guest_addr: u64, size: u64) {
        let Ok(Some((first, end))) = Self::page_range(guest_addr, size) else {
            return;
        };
        if self.boundaries.is_empty() {
            return;
        }
        self.split_at(first);
        self.split_at(end);
        for (_, ids) in self.boundaries.range_mut(first..end) {
            ids.retain(|&x| x != id);
        }
        self.coalesce(first, end);
    }

    pub fn overlapping(&self, guest_addr: u64, size: u64) -> Result<Vec<u64>, GgvlError> {
        let Some((first, end)) = Self::page_range(guest_addr, size)? else {
            return Ok(Vec::new());
        };
        // The predecessor supplies the membership at the first page;
        // subsequent boundaries are visited in original page order.
        let initial = self.boundaries.range(..=first).next_back().map(|(_, ids)| ids);
        let runs = initial.into_iter().chain(
            self.boundaries.range((Excluded(first), Included(end - 1))).map(|(_, ids)| ids)
        );
        const LINEAR_LIMIT: usize = 16;
        let mut out = Vec::new();
        let mut seen: Option<HashSet<u64>> = None;
        for ids in runs {
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
        let mut reference = BTreeMap::<u64, Vec<u64>>::new();
        for id in 1..=96u64 {
            let start = ((id * 37) % 31) * GUEST_PAGE_SIZE + id % 11;
            let len = ((id * 13) % 9 + 1) * GUEST_PAGE_SIZE;
            table.register_range(id, start, len).unwrap();
            for p in start / GUEST_PAGE_SIZE..=(start + len - 1) / GUEST_PAGE_SIZE {
                reference.entry(p).or_default().push(id);
            }
            if id % 7 == 0 {
                table.unregister_range(id, start, len);
                for ids in reference.values_mut() {
                    ids.retain(|&x| x != id);
                }
            }
        }
        for first in 0..40u64 {
            for count in 1..=12u64 {
                let mut expected = Vec::new();
                for (_, ids) in reference.range(first..=first + count - 1) {
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

    #[test]
    fn large_contiguous_buffers_need_only_boundary_entries() {
        let mut table = GuestMemoryPageTable::default();
        let start = 1u64 << 36;
        let size = 1u64 << 30;
        table.register_range(1, start, size).unwrap();
        assert_eq!(table.boundaries.len(), 2);
        table.register_range(2, start + size / 4, size / 2).unwrap();
        assert_eq!(table.boundaries.len(), 4);
        assert_eq!(table.overlapping(start, size).unwrap(), vec![1, 2]);
        table.unregister_range(2, start + size / 4, size / 2);
        assert_eq!(table.boundaries.len(), 2);
        table.unregister_range(1, start, size);
        assert!(table.boundaries.is_empty());
    }

    #[test]
    fn partial_unregister_and_reregister_preserve_page_order() {
        let p = GUEST_PAGE_SIZE;
        let mut table = GuestMemoryPageTable::default();
        table.register_range(1, p, 4 * p).unwrap();
        table.register_range(2, p, 4 * p).unwrap();
        table.unregister_range(1, 2 * p + 1, 2 * p - 1);
        assert_eq!(table.overlapping(p, p).unwrap(), vec![1, 2]);
        assert_eq!(table.overlapping(2 * p, 2 * p).unwrap(), vec![2]);
        table.register_range(1, 2 * p, 2 * p).unwrap();
        assert_eq!(table.overlapping(2 * p, 2 * p).unwrap(), vec![2, 1]);
        assert_eq!(table.overlapping(p, 4 * p).unwrap(), vec![1, 2]);
        table.unregister_range(999, 0, 10 * p);
        assert_eq!(table.boundaries.len(), 4);
    }

    #[test]
    fn sparse_interval_mutations_match_page_reference() {
        let p = GUEST_PAGE_SIZE;
        let mut table = GuestMemoryPageTable::default();
        let mut reference: Vec<Vec<u64>> = vec![Vec::new(); 64];
        let mut seed = 0x1248_ace0_u64;
        for step in 0..1200 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let id = (seed >> 32) % 25;
            let first = ((seed >> 16) % 55) as usize;
            let count = ((seed >> 8) % 9 + 1) as usize;
            let add = (seed >> 48) & 1 != 0;
            if add {
                table.register_range(id, first as u64 * p + 3, count as u64 * p - 3).unwrap();
            } else {
                table.unregister_range(id, first as u64 * p + 3, count as u64 * p - 3);
            }
            for ids in &mut reference[first..first + count] {
                if add {
                    if !ids.contains(&id) { ids.push(id); }
                } else {
                    ids.retain(|&x| x != id);
                }
            }
            for i in 0..64 {
                assert_eq!(table.overlapping(i as u64 * p, p).unwrap(), reference[i], "step {step}, page {i}");
            }
            let mut expected = Vec::new();
            for ids in &reference[first..first + count] {
                for &id in ids {
                    if !expected.contains(&id) { expected.push(id); }
                }
            }
            assert_eq!(table.overlapping(first as u64 * p, count as u64 * p).unwrap(), expected);
            let mut previous: &[u64] = &[];
            for ids in table.boundaries.values() {
                assert_ne!(previous, ids.as_slice(), "noncanonical adjacent runs");
                previous = ids;
            }
            assert!(previous.is_empty());
        }
    }

    #[test]
    fn maximum_address_and_empty_operations_are_safe() {
        let mut table = GuestMemoryPageTable::default();
        table.register_range(1, u64::MAX - 1, 1).unwrap();
        assert_eq!(table.overlapping(u64::MAX - 1, 1).unwrap(), vec![1]);
        assert_eq!(table.register_range(2, u64::MAX, 1), Err(GgvlError::AddressOverflow));
        table.unregister_range(1, u64::MAX, 1);
        assert_eq!(table.overlapping(u64::MAX - 1, 1).unwrap(), vec![1]);
        table.register_range(3, u64::MAX, 0).unwrap();
        table.unregister_range(1, u64::MAX - 1, 1);
        assert!(table.boundaries.is_empty());
    }
}
