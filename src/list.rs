//! The index-based free list used by each [`FreeArea`].
//!
//! Mirrors the kernel's `struct list_head buddy_list` embedded in
//! `struct page`: nodes are identified by their index into the page
//! descriptor array, and the links live in `Page::next`/`Page::prev`. The
//! head is kept out of line in [`FreeList`].
//!
//! Unlike C, "unlinked" is expressed with `Option<Link>` instead of a
//! `NULL` sentinel: an empty list and an unlinked node are `None`. [`Link`]
//! biases the page index by one so that `Option<Link>` uses the
//! `NonZeroUsize` niche and costs exactly as much as a raw index; a plain
//! `Option<usize>` would add a second machine word per link and grow
//! `Page` from 24 to 40 bytes on 64-bit targets.
//!
//! The representation is a circular doubly linked list: an empty list has
//! `head == None`; otherwise `head` points at one node and `prev(head)` is
//! the tail. All operations are O(1), including [`FreeList::remove`], which
//! the allocator needs to unlink an arbitrary *buddy* block during
//! coalescing, exactly like the kernel's `list_del(&buddy->lru)`.

use core::num::NonZeroUsize;

use crate::page::Page;

/// An index into the page descriptor array, biased by one.
///
/// The bias gives the type a niche so that `Option<Link>` is exactly one
/// machine word wide. This replaces the C-style `NULL` sentinel while
/// keeping the page descriptor as compact as with a raw index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Link(NonZeroUsize);

impl Link {
    /// Wraps `index`.
    ///
    /// The biased value cannot overflow: a page array is bounded by
    /// `isize::MAX` bytes, so every valid index plus one fits in a
    /// `NonZeroUsize`.
    pub(crate) fn new(index: usize) -> Self {
        let biased = index
            .checked_add(1)
            .expect("page index does not fit in a Link");
        Self(NonZeroUsize::new(biased).expect("page index does not fit in a Link"))
    }

    /// Returns the page index.
    pub(crate) fn index(self) -> usize {
        self.0.get() - 1
    }
}

/// The head of a circular doubly linked list of page indices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FreeList {
    head: Option<Link>,
}

impl FreeList {
    pub(crate) const fn new() -> Self {
        Self { head: None }
    }

    /// Inserts `idx` at the front (mirrors `list_add`).
    pub(crate) fn push_front(&mut self, pages: &mut [Page], idx: usize) {
        debug_assert!(idx < pages.len());
        let link = Link::new(idx);

        match self.head {
            None => {
                pages[idx].next = Some(link);
                pages[idx].prev = Some(link);
            }
            Some(head) => {
                let tail = pages[head.index()].prev.expect("linked node has a prev");
                pages[idx].next = Some(head);
                pages[idx].prev = Some(tail);
                pages[head.index()].prev = Some(link);
                pages[tail.index()].next = Some(link);
            }
        }

        self.head = Some(link);
    }

    /// Inserts `idx` at the back (mirrors `list_add_tail`).
    ///
    /// TODO(antifrag): the allocator currently always inserts at the
    /// front; use this for the kernel's `buddy_merge_likely` placement
    /// policy (`add_to_free_list_tail`).
    #[allow(dead_code)]
    pub(crate) fn push_back(&mut self, pages: &mut [Page], idx: usize) {
        debug_assert!(idx < pages.len());
        let link = Link::new(idx);

        match self.head {
            None => {
                pages[idx].next = Some(link);
                pages[idx].prev = Some(link);
                self.head = Some(link);
            }
            Some(head) => {
                let tail = pages[head.index()].prev.expect("linked node has a prev");
                pages[idx].next = Some(head);
                pages[idx].prev = Some(tail);
                pages[head.index()].prev = Some(link);
                pages[tail.index()].next = Some(link);
            }
        }
    }

    /// Unlinks `idx`, which must currently be on this list (mirrors
    /// `list_del`).
    pub(crate) fn remove(&mut self, pages: &mut [Page], idx: usize) {
        debug_assert!(idx < pages.len());
        let link = Link::new(idx);
        let next = pages[idx].next.expect("removed node is linked");
        let prev = pages[idx].prev.expect("removed node is linked");

        if next == link {
            debug_assert_eq!(self.head, Some(link));
            self.head = None;
        } else {
            pages[next.index()].prev = Some(prev);
            pages[prev.index()].next = Some(next);
            if self.head == Some(link) {
                self.head = Some(next);
            }
        }

        pages[idx].next = None;
        pages[idx].prev = None;
    }

    /// Removes and returns the front node.
    pub(crate) fn pop_front(&mut self, pages: &mut [Page]) -> Option<usize> {
        let head = self.head?;
        let idx = head.index();
        self.remove(pages, idx);
        Some(idx)
    }

    /// Iterates the list from front to back.
    pub(crate) fn iter<'p>(&self, pages: &'p [Page]) -> FreeListIter<'p> {
        FreeListIter {
            pages,
            head: self.head,
            next: self.head,
        }
    }
}

/// Iterator over the page indices of a [`FreeList`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct FreeListIter<'p> {
    pages: &'p [Page],
    head: Option<Link>,
    next: Option<Link>,
}

impl Iterator for FreeListIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let current = self.next?;
        let idx = current.index();
        let following = self.pages[idx].next.expect("linked node has a next");
        self.next = if Some(following) == self.head {
            None
        } else {
            Some(following)
        };
        Some(idx)
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec;
    use alloc::vec::Vec;
    use core::mem::size_of;

    use super::*;

    fn pages() -> [Page; 8] {
        [Page::EMPTY; 8]
    }

    fn collect(list: &FreeList, pages: &[Page]) -> Vec<usize> {
        list.iter(pages).collect()
    }

    #[test]
    fn link_is_niche_optimized() {
        assert_eq!(size_of::<Option<Link>>(), size_of::<usize>());
        // The links must not grow the page descriptor: 8 bytes of flags,
        // order and refcount plus two machine-word links.
        assert_eq!(size_of::<Page>(), 8 + 2 * size_of::<usize>());
    }

    #[test]
    fn link_round_trips() {
        assert_eq!(Link::new(0).index(), 0);
        assert_eq!(Link::new(1).index(), 1);
        assert_eq!(Link::new(usize::MAX - 1).index(), usize::MAX - 1);
    }

    #[test]
    fn new_list_is_empty() {
        let mut list = FreeList::new();
        let mut pages = pages();
        assert_eq!(collect(&list, &pages), vec![]);
        assert_eq!(list.pop_front(&mut pages), None);
    }

    #[test]
    fn push_front_pop_front_is_lifo() {
        let mut list = FreeList::new();
        let mut pages = pages();
        list.push_front(&mut pages, 1);
        list.push_front(&mut pages, 2);
        list.push_front(&mut pages, 3);
        assert_eq!(collect(&list, &pages), vec![3, 2, 1]);
        assert_eq!(list.pop_front(&mut pages), Some(3));
        assert_eq!(list.pop_front(&mut pages), Some(2));
        assert_eq!(list.pop_front(&mut pages), Some(1));
        assert_eq!(list.pop_front(&mut pages), None);
    }

    #[test]
    fn push_back_pop_front_is_fifo() {
        let mut list = FreeList::new();
        let mut pages = pages();
        list.push_back(&mut pages, 1);
        list.push_back(&mut pages, 2);
        list.push_back(&mut pages, 3);
        assert_eq!(collect(&list, &pages), vec![1, 2, 3]);
        assert_eq!(list.pop_front(&mut pages), Some(1));
        assert_eq!(list.pop_front(&mut pages), Some(2));
        assert_eq!(list.pop_front(&mut pages), Some(3));
        assert_eq!(list.pop_front(&mut pages), None);
    }

    #[test]
    fn mixed_pushes_keep_order() {
        let mut list = FreeList::new();
        let mut pages = pages();
        list.push_back(&mut pages, 1);
        list.push_front(&mut pages, 0);
        list.push_back(&mut pages, 2);
        assert_eq!(collect(&list, &pages), vec![0, 1, 2]);
    }

    #[test]
    fn remove_front_middle_back() {
        let mut list = FreeList::new();
        let mut pages = pages();
        for idx in 0..5 {
            list.push_back(&mut pages, idx);
        }
        list.remove(&mut pages, 0);
        assert_eq!(collect(&list, &pages), vec![1, 2, 3, 4]);
        list.remove(&mut pages, 2);
        assert_eq!(collect(&list, &pages), vec![1, 3, 4]);
        list.remove(&mut pages, 4);
        assert_eq!(collect(&list, &pages), vec![1, 3]);
    }

    #[test]
    fn remove_last_element_empties_list() {
        let mut list = FreeList::new();
        let mut pages = pages();
        list.push_front(&mut pages, 7);
        list.remove(&mut pages, 7);
        assert_eq!(list.iter(&pages).count(), 0);
        assert_eq!(list.pop_front(&mut pages), None);
    }

    #[test]
    fn removed_nodes_are_unlinked() {
        let mut list = FreeList::new();
        let mut pages = pages();
        list.push_back(&mut pages, 1);
        list.push_back(&mut pages, 2);
        list.remove(&mut pages, 1);
        assert!(pages[1].next.is_none());
        assert!(pages[1].prev.is_none());
        assert_eq!(pages[2].next, Some(Link::new(2)));
        assert_eq!(pages[2].prev, Some(Link::new(2)));
    }

    #[test]
    fn reinsert_after_remove_works() {
        let mut list = FreeList::new();
        let mut pages = pages();
        list.push_front(&mut pages, 1);
        list.remove(&mut pages, 1);
        list.push_front(&mut pages, 1);
        assert_eq!(collect(&list, &pages), vec![1]);
        list.push_back(&mut pages, 2);
        assert_eq!(collect(&list, &pages), vec![1, 2]);
    }

    #[test]
    fn iterating_empty_list_yields_nothing() {
        let list = FreeList::new();
        let pages = pages();
        assert_eq!(list.iter(&pages).count(), 0);
    }
}
