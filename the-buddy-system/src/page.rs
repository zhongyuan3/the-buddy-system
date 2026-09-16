//! The page descriptor, the counterpart of the kernel's `struct page`.

use bitflags::bitflags;

use crate::list::Link;

bitflags! {
    /// Per-page flags, modeled after the kernel's `PG_*` flags
    /// (`include/linux/page-flags.h`).
    #[repr(transparent)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PageFlags: u8 {
        /// The page is the head of a block currently on a free list
        /// (mirrors `PG_buddy` / `PageBuddy`).
        const BUDDY = 0x1;
    }
}

/// A physical page descriptor, mirroring `struct page`
/// (`include/linux/mm_types.h`).
///
/// Only the fields the buddy allocator needs are modeled:
///
/// - `flags`/`order` mirror `page->flags` and the `page->private` order
///   stored for free pages (`PageBuddy` + `buddy_order`). Both are only
///   meaningful for the *head* page of a block; the kernel and this crate
///   only ever inspect block heads, because every block is aligned.
/// - `refcount` mirrors `page->_refcount`. The buddy allocator sets it to
///   one when a block is handed out and requires it to be one when the
///   block is returned, which turns double frees into
///   [`Error::InvalidFree`](crate::error::Error::InvalidFree).
/// - `next`/`prev` mirror the `page->buddy_list` links. Instead of raw
///   pointers they store niche-optimized optional page indices (`Link`
///   biases the index by one so `Option<Link>` is one machine word),
///   keeping the whole allocator free of `unsafe` and of a `NULL`
///   sentinel.
///
/// TODO(page): grow towards the full `struct page` (`_mapcount`, `mapping`,
/// `lru`, `PG_*` flags beyond `BUDDY`, ...) as the slab, page cache and
/// rmap layers need them; keep the buddy fields first so the hot paths
/// stay compact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub(crate) flags: PageFlags,
    pub(crate) order: u8,
    pub(crate) refcount: u32,
    pub(crate) next: Option<Link>,
    pub(crate) prev: Option<Link>,
}

impl Page {
    /// A pristine descriptor: not free, not allocated, not linked.
    pub const EMPTY: Page = Page {
        flags: PageFlags::empty(),
        order: 0,
        refcount: 0,
        next: None,
        prev: None,
    };

    /// Creates a pristine descriptor.
    pub const fn new() -> Self {
        Self::EMPTY
    }

    /// Returns the page's flags.
    pub const fn flags(&self) -> PageFlags {
        self.flags
    }

    /// Returns the block order stored on a free head page.
    pub const fn order(&self) -> u8 {
        self.order
    }

    /// Returns the page's reference count.
    pub const fn refcount(&self) -> u32 {
        self.refcount
    }

    /// Returns `true` if the page is the head of a block on a free list.
    pub const fn is_free(&self) -> bool {
        self.flags.contains(PageFlags::BUDDY)
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_page_is_pristine() {
        let page = Page::EMPTY;
        assert_eq!(page.flags(), PageFlags::empty());
        assert_eq!(page.order(), 0);
        assert_eq!(page.refcount(), 0);
        assert!(!page.is_free());
        assert_eq!(page, Page::new());
        assert_eq!(page, Page::default());
    }

    #[test]
    fn buddy_flag_marks_free_heads() {
        let mut page = Page::EMPTY;
        page.flags.insert(PageFlags::BUDDY);
        page.order = 3;
        assert!(page.is_free());
        assert_eq!(page.order(), 3);

        page.flags.remove(PageFlags::BUDDY);
        assert!(!page.is_free());
    }

    #[test]
    fn page_is_copy_and_eq() {
        let a = Page::EMPTY;
        let b = a;
        assert_eq!(a, b);

        let mut c = Page::EMPTY;
        c.refcount = 1;
        assert_ne!(a, c);
    }
}
