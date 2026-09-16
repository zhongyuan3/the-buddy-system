//! The per-order free area, the counterpart of the kernel's `struct free_area`.

use crate::list::FreeList;
use crate::list::FreeListIter;
use crate::page::Page;
use crate::page::PageFlags;

/// The free list of one order, mirroring `struct free_area`
/// (`include/linux/mmzone.h`).
///
/// The kernel's `free_area` holds one list per migration type; migration
/// types are not implemented yet, so a single list is kept. Adding them
/// later only turns `free_list` into an array and requires a migration type
/// argument in the allocator, exactly like `migratetype` in
/// `__rmqueue_smallest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreeArea {
    // TODO(migratetype): split into `free_list: [FreeList; MIGRATE_TYPES]`,
    // one list per migration type, exactly like the kernel's
    // `struct free_area`; `alloc_pages` then needs the `__rmqueue_fallback`
    // walk over the remaining lists.
    pub(crate) free_list: FreeList,
    pub(crate) nr_free: usize,
    // TODO(bitmap): add the per-order bitmap of buddy pairs
    // (`free_area.bitmap`), consulted by `buddy_merge_likely` to skip
    // cache-cold buddy checks during coalescing.
}

impl FreeArea {
    pub(crate) const fn new() -> Self {
        Self {
            free_list: FreeList::new(),
            nr_free: 0,
        }
    }

    /// Returns the number of free blocks of this order.
    ///
    /// Counts *blocks*, not pages, exactly like the kernel's
    /// `free_area.nr_free` (the value shown per order by `/proc/buddyinfo`).
    /// The page count is tracked by [`Buddy::nr_free`].
    ///
    /// [`Buddy::nr_free`]: crate::buddy::Buddy::nr_free
    pub const fn nr_free(&self) -> usize {
        self.nr_free
    }

    /// Pushes a block head at the front of the list and bumps the count.
    pub(crate) fn push_front(&mut self, pages: &mut [Page], idx: usize) {
        self.free_list.push_front(pages, idx);
        self.nr_free += 1;
    }

    /// Unlinks a block head and drops the count.
    ///
    /// Clearing the `BUDDY` flag and the order mirrors the kernel's
    /// `del_page_from_free_list` (`__ClearPageBuddy` plus
    /// `set_page_private(page, 0)`).
    pub(crate) fn remove(&mut self, pages: &mut [Page], idx: usize) {
        debug_assert!(pages[idx].is_free());
        self.free_list.remove(pages, idx);
        self.nr_free -= 1;
        clear_free_head(&mut pages[idx]);
    }

    /// Pops the front block head of the given `order`, if any.
    pub(crate) fn pop_front(&mut self, pages: &mut [Page], order: usize) -> Option<usize> {
        let idx = self.free_list.pop_front(pages)?;
        debug_assert!(pages[idx].is_free());
        debug_assert_eq!(pages[idx].order as usize, order);
        self.nr_free -= 1;
        clear_free_head(&mut pages[idx]);
        Some(idx)
    }

    /// Iterates the block heads of this order, front to back.
    pub(crate) fn iter<'p>(&self, pages: &'p [Page]) -> FreeListIter<'p> {
        self.free_list.iter(pages)
    }
}

/// Clears the free-head state of an unlinked block head.
fn clear_free_head(page: &mut Page) {
    page.flags.remove(PageFlags::BUDDY);
    page.order = 0;
}
