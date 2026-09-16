//! The buddy allocator core.
//!
//! This module mirrors the data structures and algorithms of the kernel's
//! buddy allocator in `mm/page_alloc.c` and `include/linux/mmzone.h`:
//!
//! - [`Buddy::free_range`] mirrors `free_low_memory_core_early` /
//!   `__free_pages_memory` and bootstraps the allocator from memory
//!   discovered at boot (typically the free ranges of [`Memblock`]).
//! - `expand` (private) mirrors the kernel's `expand`.
//! - `free_one_page` (private) mirrors the kernel's `__free_one_page`.
//! - [`Buddy::alloc_pages`] mirrors `__rmqueue_smallest` plus `expand`.
//! - [`Buddy::free_pages`] mirrors `__free_pages` followed by
//!   `free_pages_prepare`'s state checks.
//!
//! [`Memblock`]: the_memblock::memblock::Memblock

use core::cmp::max;
use core::cmp::min;
use core::marker::PhantomData;
use core::mem::align_of;
use core::ptr::NonNull;

use crate::error::Error;
use crate::free_area::FreeArea;
use crate::order::pages_in_order;
use crate::page::Page;
use crate::page::PageFlags;
use crate::pfn::PageFrame;

/// A buddy allocator managing a contiguous range of physical page frames.
///
/// The allocator manages `nr_pages` page frames starting at `base`. The
/// descriptors are supplied by the caller (the counterpart of the kernel's
/// `vmemmap`/`mem_map` array), so the allocator itself never allocates and
/// a kernel can place the array wherever its early boot code reserved room
/// for it.
///
/// `MAX_ORDER` is the number of free areas, exactly like the kernel's
/// `MAX_ORDER`: valid orders are `0..MAX_ORDER` and the largest block that
/// can be allocated is `1 << (MAX_ORDER - 1)` pages
/// (`MAX_ORDER_NR_PAGES`). The kernel's typical `MAX_ORDER` of 11 therefore
/// means orders 0 through 10 with 4 MiB blocks on 4 KiB pages.
///
/// # Descriptor storage
///
/// The descriptor array is stored as an address and a length, not as a
/// slice: every method rebuilds the slice from the stored pointer when it
/// runs, which is the library's equivalent of entering the caller's
/// critical section. This matches how a kernel keeps its page descriptors:
/// a `vmemmap` array at a fixed address with no borrow that could be named.
///
/// - [`Buddy::new`] takes `&'a mut [Page]` for tests and embedded users and
///   keeps the borrow alive through a zero-sized marker, which is safe.
/// - [`Buddy::from_raw_parts`] takes a raw pointer and returns a `'static`
///   allocator for kernel use; it is `unsafe`, because only the caller can
///   promise that the array outlives the allocator and that all access to
///   it is externally synchronized (for example by keeping the allocator
///   in a spin lock).
///
/// # Static initialization
///
/// A static needs a constant initializer, but the descriptor address and
/// page count only become known at boot. [`Buddy::uninit`] is a `const`
/// placeholder for exactly that: it manages zero pages, every operation
/// that needs descriptors reports [`Error::Uninitialized`] instead of
/// touching the dangling internal pointer, and the one-shot
/// [`Buddy::init`] fills it in. With a lock that has a `const fn new`, the
/// whole static is safe to define:
///
/// ```ignore
/// static BUDDY: SpinLock<Buddy<'static, u64, 11>> =
///     SpinLock::new(Buddy::uninit());
///
/// fn boot(vmemmap: NonNull<Page>, nr_pages: usize) -> Result<(), Error> {
///     // SAFETY: `vmemmap` names the descriptor array, which outlives the
///     // system, and all access goes through the lock.
///     unsafe { BUDDY.lock().init(vmemmap, nr_pages, 0, 0x1000) }
/// }
/// ```
///
/// # Address model
///
/// `A` is the physical address type. As in the kernel, where `phys_addr_t`
/// can be wider than a pointer (`CONFIG_PHYS_ADDR_T_64BIT` on 32-bit x86
/// with PAE), absolute addresses are never narrowed to `usize`: only page
/// offsets relative to `base` are, through the checked
/// [`PageFrame::try_to_usize`]. All array indexing and buddy arithmetic
/// (`idx ^ (1 << order)`) happens in `usize` index space, which is
/// isomorphic to PFN space because `base` is aligned to the largest block.
///
/// # Kernel layer
///
/// This type models one zone's buddy allocator. The layers the kernel
/// stacks on top of it — zones with `GFP_*` allocation flags and
/// watermarks (`__zone_watermark_ok`), and per-CPU pagesets
/// (`struct per_cpu_pages`) — are not implemented yet.
///
/// # Examples
///
/// ```
/// use the_buddy_system::Buddy;
/// use the_buddy_system::Page;
///
/// let mut pages = [Page::EMPTY; 64];
/// let mut buddy = Buddy::<u64, 4>::new(0, 0x1000, &mut pages).unwrap();
///
/// // Feed the arena the memory that is actually free.
/// buddy.free_range(0, 64 * 0x1000).unwrap();
/// assert_eq!(buddy.nr_free(), 64);
///
/// // Allocate a 4-page block and return it.
/// let addr = buddy.alloc_pages(2).unwrap();
/// assert_eq!(addr % (4 * 0x1000), 0);
/// buddy.free_pages(addr, 2).unwrap();
/// assert_eq!(buddy.nr_free(), 64);
/// ```
#[derive(Debug)]
// TODO(zone): wrap this in a zone type adding `GFP_*` flag filtering,
// watermark checks (`__zone_watermark_ok`) and per-CPU pagesets
// (`struct per_cpu_pages`).
pub struct Buddy<'a, A: PageFrame, const MAX_ORDER: usize> {
    page_size: A,
    base_pfn: A,
    /// Address of the page descriptor array (the `vmemmap` counterpart).
    /// The slice is never stored; each method rebuilds it from this pointer
    /// through `pages_mut`/`pages_ref`.
    pages: NonNull<Page>,
    /// Number of page descriptors, i.e. the array length.
    nr_pages: usize,
    areas: [FreeArea; MAX_ORDER],
    nr_free: usize,
    /// Marker for the borrow held by [`Buddy::new`]; raw construction
    /// ([`Buddy::from_raw_parts`]) uses `'static`.
    _pages: PhantomData<&'a mut [Page]>,
}

// SAFETY: `Buddy` holds no thread-affine state, and the descriptor array is
// only dereferenced inside method calls. Every method's contract requires
// the caller to serialize access to the descriptors (see
// `from_raw_parts`), so moving the allocator between threads cannot create
// a data race.
unsafe impl<A: PageFrame + Send, const MAX_ORDER: usize> Send for Buddy<'_, A, MAX_ORDER> {}

/// Rebuilds the descriptor slice for the duration of one method call.
///
/// The slice is intentionally not stored on the allocator: a kernel places
/// the page descriptors at a fixed address (`vmemmap`) that outlives any
/// nameable borrow, so the allocator keeps only the address and length and
/// reconstructs the slice as it enters each externally synchronized
/// critical section.
///
/// # Safety
///
/// `ptr` must point to `len` initialized, properly aligned `Page`
/// descriptors that are not reached through any other alias while the
/// returned reference is alive.
unsafe fn pages_mut<'p>(ptr: NonNull<Page>, len: usize) -> &'p mut [Page] {
    // SAFETY: the caller guarantees pointer validity and exclusivity.
    unsafe { core::slice::from_raw_parts_mut(ptr.as_ptr(), len) }
}

/// Like [`pages_mut`], for read-only access.
///
/// # Safety
///
/// Same contract as [`pages_mut`]; the caller must additionally serialize
/// against writers.
unsafe fn pages_ref<'p>(ptr: NonNull<Page>, len: usize) -> &'p [Page] {
    // SAFETY: the caller guarantees pointer validity.
    unsafe { core::slice::from_raw_parts(ptr.as_ptr(), len) }
}

impl<'a, A: PageFrame, const MAX_ORDER: usize> Buddy<'a, A, MAX_ORDER> {
    /// Creates an allocator managing the page frames of `pages`.
    ///
    /// `base` is the physical address of the first managed page and
    /// `page_size` its size. All descriptors are reset to [`Page::EMPTY`];
    /// no page is free until a range is handed to [`Buddy::free_range`] or
    /// [`Buddy::free_memblock`], mirroring the kernel, where memory is only
    /// freed into the buddy allocator by `memblock_free_all`.
    ///
    /// The borrow guarantees that the descriptor array outlives the
    /// allocator; kernel code that cannot name such a borrow uses
    /// [`Buddy::from_raw_parts`] instead.
    ///
    /// The arena must satisfy the invariants the kernel guarantees for
    /// zones:
    ///
    /// - the descriptor array holds at least the largest block,
    ///   `1 << (MAX_ORDER - 1)` pages (`MAX_ORDER_NR_PAGES`);
    /// - its length survives a `from_usize`/`try_to_usize` round trip, so
    ///   every index is exact;
    /// - `base` is aligned to the largest block, which makes index-space
    ///   alignment coincide with PFN-space alignment (`zone_start_pfn` is
    ///   `pageblock_nr_pages` aligned for the same reason);
    /// - the managed range does not wrap around the top of the address
    ///   space of `A`.
    ///
    /// Mirrors `free_area_init` and `zone_init_free_lists` for one zone.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPageSize`] if `page_size` is zero, not a
    /// power of two, or not representable as `usize`;
    /// [`Error::InvalidGeometry`] if any of the geometry invariants above
    /// is violated.
    pub fn new(base: A, page_size: A, pages: &'a mut [Page]) -> Result<Self, Error> {
        let nr_pages = pages.len();
        if nr_pages == 0 {
            return Err(Error::InvalidGeometry);
        }
        let ptr = NonNull::new(pages.as_mut_ptr()).expect("slice pointers are never null");

        let mut buddy = Self::uninit();
        // SAFETY: `pages` is a valid, exclusively borrowed slice for `'a`,
        // and the returned allocator holds that borrow through `PhantomData`.
        unsafe { buddy.set_state(ptr, nr_pages, base, page_size) }?;
        Ok(buddy)
    }

    /// A `const` placeholder for static definitions.
    ///
    /// The placeholder manages zero pages. Until [`Buddy::init`] runs, the
    /// queries report zeroes and the operations that need descriptors
    /// (`alloc_pages`, `free_pages`, `free_range`, `validate`) report
    /// [`Error::Uninitialized`]; the dangling internal pointer is never
    /// dereferenced. This makes `Buddy::uninit()` sound as the initializer
    /// of a static that is filled in at boot.
    pub const fn uninit() -> Self {
        Self {
            page_size: A::ZERO,
            base_pfn: A::ZERO,
            pages: NonNull::dangling(),
            nr_pages: 0,
            areas: [FreeArea::new(); MAX_ORDER],
            nr_free: 0,
            _pages: PhantomData,
        }
    }

    /// Returns `true` once a constructor or [`Buddy::init`] has supplied a
    /// descriptor array.
    pub const fn is_initialized(&self) -> bool {
        self.nr_pages != 0
    }

    /// Initializes an allocator created by [`Buddy::uninit`].
    ///
    /// Validates the arena geometry, resets the descriptors to
    /// [`Page::EMPTY`] and starts managing the array, exactly like
    /// [`Buddy::from_raw_parts`], but in place. On failure the allocator is
    /// left in the placeholder state, so a failed boot probe can be retried
    /// with a corrected geometry.
    ///
    /// # Safety
    ///
    /// Same contract as [`Buddy::from_raw_parts`]: `ptr` must point to
    /// `nr_pages` properly aligned `Page` descriptors that stay valid and
    /// exclusively accessible (under the caller's synchronization) for as
    /// long as the allocator is used.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyInitialized`] if the allocator already manages
    /// pages; otherwise the same errors as [`Buddy::new`].
    pub unsafe fn init(
        &mut self,
        ptr: NonNull<Page>,
        nr_pages: usize,
        base: A,
        page_size: A,
    ) -> Result<(), Error> {
        if self.is_initialized() {
            return Err(Error::AlreadyInitialized);
        }
        // SAFETY: the caller guarantees the descriptor contract.
        unsafe { self.set_state(ptr, nr_pages, base, page_size) }
    }

    /// Shared body of every raw initialization: validates the geometry and
    /// resets the descriptors to [`Page::EMPTY`].
    ///
    /// # Safety
    ///
    /// `ptr` must point to `nr_pages` valid, properly aligned `Page`
    /// descriptors that stay exclusively accessible for the whole call and
    /// for every later method call; see [`Buddy::from_raw_parts`].
    unsafe fn set_state(
        &mut self,
        ptr: NonNull<Page>,
        nr_pages: usize,
        base: A,
        page_size: A,
    ) -> Result<(), Error> {
        if MAX_ORDER == 0 || MAX_ORDER > usize::BITS as usize {
            return Err(Error::InvalidGeometry);
        }
        if (ptr.as_ptr() as usize) % align_of::<Page>() != 0 {
            return Err(Error::InvalidPageArray);
        }

        let page_size_usize = page_size.try_to_usize().ok_or(Error::InvalidPageSize)?;
        if page_size_usize == 0 || !page_size_usize.is_power_of_two() {
            return Err(Error::InvalidPageSize);
        }

        let max_block = pages_in_order((MAX_ORDER - 1) as u8);
        if nr_pages < max_block {
            return Err(Error::InvalidGeometry);
        }

        // The page count must be representable as an index and as a PFN:
        // this rejects narrow address types whose `from_usize` saturates.
        let len = A::from_usize(nr_pages);
        if len.try_to_usize() != Some(nr_pages) {
            return Err(Error::InvalidGeometry);
        }

        let base_pfn = A::pfn_down(base, page_size);

        // The managed range may not wrap around the top of the address
        // space, and the last page's address must be representable.
        if base_pfn > A::MAX - len {
            return Err(Error::InvalidGeometry);
        }
        let max_pfn = A::pfn_down(A::MAX, page_size);
        let last = len - A::from_usize(1);
        if base_pfn > max_pfn || max_pfn - base_pfn < last {
            return Err(Error::InvalidGeometry);
        }

        // `base` must be aligned to the largest block. `pfn_to_phys` is
        // exact here because a block is at most `nr_pages` pages, which the
        // checks above proved representable.
        let block_pfn = A::from_usize(max_block);
        if block_pfn > max_pfn {
            return Err(Error::InvalidGeometry);
        }
        let block_bytes = A::pfn_to_phys(block_pfn, page_size);
        if A::align_down(base, block_bytes) != base {
            return Err(Error::InvalidGeometry);
        }

        // SAFETY: the caller guarantees validity and exclusivity.
        for page in unsafe { pages_mut(ptr, nr_pages) }.iter_mut() {
            *page = Page::EMPTY;
        }

        self.page_size = page_size;
        self.base_pfn = base_pfn;
        self.pages = ptr;
        self.nr_pages = nr_pages;
        self.areas = [FreeArea::new(); MAX_ORDER];
        self.nr_free = 0;
        Ok(())
    }

    /// Returns the physical address of the first managed page.
    ///
    /// Returns `A::ZERO` while the allocator is uninitialized, where there
    /// is no managed range at all.
    pub fn base(&self) -> A {
        if !self.is_initialized() {
            return A::ZERO;
        }
        A::pfn_to_phys(self.base_pfn, self.page_size)
    }

    /// Returns the page size the allocator was created with, or `A::ZERO`
    /// while the allocator is uninitialized.
    pub fn page_size(&self) -> A {
        self.page_size
    }

    /// Returns the number of managed pages (`zone->managed_pages`), zero
    /// while the allocator is uninitialized.
    pub fn managed_pages(&self) -> usize {
        self.nr_pages
    }

    /// Returns the number of free pages (`zone->free_pages`).
    pub fn nr_free(&self) -> usize {
        self.nr_free
    }

    /// Returns the number of free blocks of `order`
    /// (`zone->free_area[order].nr_free`), or `None` if `order` is not
    /// below `MAX_ORDER`.
    pub fn nr_free_blocks(&self, order: u8) -> Option<usize> {
        if (order as usize) < MAX_ORDER {
            Some(self.areas[order as usize].nr_free())
        } else {
            None
        }
    }

    /// Converts a page frame number to an index into `pages`.
    ///
    /// This is the only place where `A` is narrowed to `usize`. The input
    /// is a page offset *relative to the arena base*, never an absolute
    /// address, and the conversion is checked, so a physical address that
    /// does not fit in `usize` on 32-bit targets is rejected instead of
    /// truncated.
    fn index_of_pfn(&self, pfn: A) -> Result<usize, Error> {
        if pfn < self.base_pfn {
            return Err(Error::InvalidAddress);
        }
        let idx = (pfn - self.base_pfn)
            .try_to_usize()
            .ok_or(Error::InvalidAddress)?;
        if idx >= self.nr_pages {
            return Err(Error::InvalidAddress);
        }
        Ok(idx)
    }

    /// Converts an index into a page frame number. `idx` may be
    /// `nr_pages` (one past the last page).
    fn pfn_of_index(&self, idx: usize) -> A {
        debug_assert!(idx <= self.nr_pages);
        self.base_pfn + A::from_usize(idx)
    }

    /// Converts an index into a physical address (`PFN_PHYS`).
    fn addr_of_index(&self, idx: usize) -> A {
        A::pfn_to_phys(self.pfn_of_index(idx), self.page_size)
    }

    /// Converts a physical address of a page start into an index.
    fn index_of_addr(&self, addr: A) -> Result<usize, Error> {
        if A::align_down(addr, self.page_size) != addr {
            return Err(Error::InvalidAddress);
        }
        self.index_of_pfn(A::pfn_down(addr, self.page_size))
    }

    /// Returns `true` if `idx` lies inside a free block.
    ///
    /// A page is covered by a free block of order `k` if and only if
    /// `idx & !((1 << k) - 1)` is a free head of order `k`; a block of a
    /// smaller order cannot contain `idx` without starting at `idx`
    /// itself, which the `k = 0` iteration catches. Used to reject ranges
    /// that partially overlap memory that is already free.
    ///
    /// Takes the descriptor slice rather than `&self` so that callers
    /// already holding it do not construct a second, aliasing one.
    fn is_inside_free_block(pages: &[Page], idx: usize) -> bool {
        for order in 0..MAX_ORDER {
            let head = idx & !(pages_in_order(order as u8) - 1);
            let page = &pages[head];
            if page.is_free() && page.order as usize == order {
                return true;
            }
        }
        false
    }

    /// Splits the block at `idx` down to `low`, adding the upper half of
    /// each intermediate order to the free lists.
    ///
    /// Mirrors the kernel's `expand`. Takes the descriptor slice instead of
    /// rebuilding it, so that callers holding one never create a second,
    /// aliasing slice (Stacked Borrows forbids using the older one
    /// afterwards).
    fn expand(&mut self, pages: &mut [Page], idx: usize, low: u8, mut high: u8) {
        debug_assert!(high >= low);

        while high > low {
            high -= 1;
            let half = pages_in_order(high);
            let upper = idx + half;

            // TODO(guard): let `set_page_guard` consume a split block for
            // CONFIG_DEBUG_PAGEALLOC/hugetlb instead of freeing it.
            let page = &mut pages[upper];
            debug_assert!(!page.is_free());
            page.flags.insert(PageFlags::BUDDY);
            page.order = high;

            self.areas[high as usize].push_front(pages, upper);
            self.nr_free += half;
        }
    }

    /// Returns a block of at least `2^order` pages and splits it down to
    /// exactly that size.
    ///
    /// Mirrors `__rmqueue_smallest` (`del_page_from_free_list` plus
    /// `expand`) and the reference counting performed by `prep_new_page`:
    /// the returned block's head page is marked allocated with a reference
    /// count of one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOrder`] if `order >= MAX_ORDER`, or
    /// [`Error::OutOfMemory`] if no free block of that order or larger
    /// exists.
    pub fn alloc_pages(&mut self, order: u8) -> Result<A, Error> {
        if !self.is_initialized() {
            return Err(Error::Uninitialized);
        }
        if order as usize >= MAX_ORDER {
            return Err(Error::InvalidOrder);
        }

        // SAFETY: `&mut self`; access to the descriptors is externally
        // synchronized, see `from_raw_parts`. The slice is built once and
        // passed down, never rebuilt while an older one is still in use.
        let pages = unsafe { pages_mut(self.pages, self.nr_pages) };

        // TODO(migratetype): serve the requested migration type's list
        // first and fall back to the other lists when it is empty,
        // mirroring `__rmqueue_fallback`.
        for current in order as usize..MAX_ORDER {
            let Some(idx) = self.areas[current].pop_front(pages, current) else {
                continue;
            };

            self.nr_free -= pages_in_order(current as u8);
            self.expand(pages, idx, order, current as u8);

            let page = &mut pages[idx];
            page.flags.remove(PageFlags::BUDDY);
            page.order = 0;
            page.refcount = 1;
            return Ok(self.addr_of_index(idx));
        }

        Err(Error::OutOfMemory)
    }

    /// Puts the block of `2^order` pages starting at `addr` back on the
    /// free lists, coalescing it with free buddy blocks.
    ///
    /// The block must have been returned by [`Buddy::alloc_pages`] with the
    /// same order. Mirrors `__free_pages` (`put_page_testzero` plus
    /// `free_pages_prepare`'s state checks).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOrder`] if `order >= MAX_ORDER`.
    /// [`Error::InvalidAddress`] if `addr` is not page aligned, denotes a
    /// block head that is misaligned for its size, or lies outside the
    /// managed range. [`Error::InvalidFree`] if the block is already free,
    /// overlaps free memory, or is not in use.
    pub fn free_pages(&mut self, addr: A, order: u8) -> Result<(), Error> {
        if !self.is_initialized() {
            return Err(Error::Uninitialized);
        }
        if order as usize >= MAX_ORDER {
            return Err(Error::InvalidOrder);
        }

        let idx = self.index_of_addr(addr)?;
        let block = pages_in_order(order);
        if idx % block != 0 || idx + block > self.nr_pages {
            return Err(Error::InvalidAddress);
        }

        // SAFETY: `&mut self`; access to the descriptors is externally
        // synchronized, see `from_raw_parts`.
        let pages = unsafe { pages_mut(self.pages, self.nr_pages) };
        let page = &mut pages[idx];
        if page.is_free() || page.refcount != 1 {
            return Err(Error::InvalidFree);
        }
        page.refcount = 0;

        self.free_one_page(pages, idx, order)
    }

    /// Frees `[start, end)`, splitting it into the largest naturally
    /// aligned blocks. Parts outside the managed range are ignored, which
    /// makes it safe to feed whole memory ranges discovered at boot.
    ///
    /// Mirrors `free_low_memory_core_early` and its helper
    /// `__free_pages_memory`: the chunk order is limited by the index's
    /// alignment (`__ffs(start)pfn`) and by the remaining length.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFree`] if the range overlaps memory that is
    /// already free or in use.
    pub fn free_range(&mut self, start: A, end: A) -> Result<(), Error> {
        if !self.is_initialized() {
            return Err(Error::Uninitialized);
        }

        let start_pfn = max(A::pfn_up(start, self.page_size), self.base_pfn);
        let end_limit = self.pfn_of_index(self.nr_pages);
        let end_pfn = min(A::pfn_down(end, self.page_size), end_limit);

        if start_pfn >= end_pfn {
            return Ok(());
        }

        let mut idx = self.index_of_pfn(start_pfn)?;
        let end_idx = if end_pfn == end_limit {
            self.nr_pages
        } else {
            self.index_of_pfn(end_pfn)?
        };

        // SAFETY: `&mut self`; access to the descriptors is externally
        // synchronized, see `from_raw_parts`.
        let pages = unsafe { pages_mut(self.pages, self.nr_pages) };

        while idx < end_idx {
            let remaining = end_idx - idx;
            let mut order = idx.trailing_zeros();
            let max_order = (MAX_ORDER - 1) as u32;
            if order > max_order {
                order = max_order;
            }
            while pages_in_order(order as u8) > remaining {
                order -= 1;
            }

            self.free_one_page(pages, idx, order as u8)?;
            idx += pages_in_order(order as u8);
        }

        Ok(())
    }

    /// Adds the block at `idx` to the free lists, merging with free
    /// buddies until no larger block can be formed.
    ///
    /// Mirrors the kernel's `__free_one_page`. The caller must have
    /// verified that the block is fully free-able.
    fn free_one_page(&mut self, pages: &mut [Page], idx: usize, order: u8) -> Result<(), Error> {
        debug_assert_eq!(idx % pages_in_order(order), 0);

        if Self::is_inside_free_block(pages, idx) {
            return Err(Error::InvalidFree);
        }
        if pages[idx].refcount != 0 {
            return Err(Error::InvalidFree);
        }

        let mut idx = idx;
        let mut order = order;
        while (order as usize) + 1 < MAX_ORDER {
            // TODO(bitmap): consult the per-order buddy bitmap first, as
            // `buddy_merge_likely` does, to avoid touching a cache-cold
            // buddy page for every failed merge.
            let buddy = idx ^ pages_in_order(order);
            if buddy >= self.nr_pages {
                break;
            }
            let page = &pages[buddy];
            if !page.is_free() || page.order != order {
                break;
            }

            self.areas[order as usize].remove(pages, buddy);
            self.nr_free -= pages_in_order(order);
            idx &= buddy;
            order += 1;
        }

        // TODO(antifrag): insert at the tail when the merged block's
        // buddy is likely free, mirroring `buddy_merge_likely` and
        // `add_to_free_list_tail`; this keeps large blocks mergeable and
        // reduces fragmentation.
        let page = &mut pages[idx];
        page.flags.insert(PageFlags::BUDDY);
        page.order = order;
        self.areas[order as usize].push_front(pages, idx);
        self.nr_free += pages_in_order(order);
        Ok(())
    }

    /// Verifies the allocator's invariants.
    ///
    /// Checks that every listed block is aligned, marked free with the
    /// right order and reference count zero, that the block and page counts
    /// are consistent, that no free head is unlinked, and that no two free
    /// buddies could have been coalesced. This is the library counterpart
    /// of the kernel's `bad_page` checks and of the freezer selftests.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptFreeList`], [`Error::CountMismatch`] or
    /// [`Error::Uncoalesced`] describing the first violated invariant.
    pub fn validate(&self) -> Result<(), Error> {
        if !self.is_initialized() {
            return Err(Error::Uninitialized);
        }

        let mut page_total = 0usize;
        let mut listed_heads = 0usize;

        // SAFETY: read-only access to the descriptors; the caller must
        // serialize against writers (see `from_raw_parts`).
        let pages = unsafe { pages_ref(self.pages, self.nr_pages) };

        for order in 0..MAX_ORDER {
            let block = pages_in_order(order as u8);
            let mut count = 0usize;

            for idx in self.areas[order].iter(pages) {
                count += 1;
                // A cycle in a corrupted list would otherwise loop forever.
                if count > self.nr_pages {
                    return Err(Error::CorruptFreeList);
                }
                if idx % block != 0 || idx + block > self.nr_pages {
                    return Err(Error::CorruptFreeList);
                }
                let page = &pages[idx];
                if !page.is_free() || page.order as usize != order || page.refcount != 0 {
                    return Err(Error::CorruptFreeList);
                }
            }

            if count != self.areas[order].nr_free() {
                return Err(Error::CountMismatch);
            }
            listed_heads += count;
            page_total += count * block;

            // Two free buddies at `order` would have been coalesced into
            // `order + 1`, so they cannot exist below the largest order.
            if order + 1 < MAX_ORDER {
                for idx in self.areas[order].iter(pages) {
                    let buddy = idx ^ block;
                    if buddy < self.nr_pages {
                        let page = &pages[buddy];
                        if page.is_free() && page.order as usize == order {
                            return Err(Error::Uncoalesced);
                        }
                    }
                }
            }
        }

        if page_total != self.nr_free {
            return Err(Error::CountMismatch);
        }

        let flagged = pages.iter().filter(|page| page.is_free()).count();
        if flagged != listed_heads {
            return Err(Error::CorruptFreeList);
        }

        Ok(())
    }
}

impl<A: PageFrame, const MAX_ORDER: usize> Buddy<'static, A, MAX_ORDER> {
    /// Creates a `'static` allocator over a page descriptor array given by
    /// address and length.
    ///
    /// This is the constructor for kernel use. The kernel's page
    /// descriptors (`vmemmap`/`mem_map`) live at a fixed address for the
    /// whole lifetime of the system, so there is no borrow to tie the
    /// allocator to; the descriptors usually also appear in structures that
    /// are created once and outlive everything else.
    ///
    /// The descriptors must be initialized; the constructor resets them to
    /// [`Page::EMPTY`] first, so passing a freshly zeroed array is fine.
    ///
    /// # Safety
    ///
    /// - `ptr` must point to `nr_pages` properly aligned `Page` descriptors
    ///   that stay valid for reads and writes for as long as the allocator
    ///   (or anything derived from it) is used; the kernel's `vmemmap`
    ///   satisfies this by construction.
    /// - The descriptors must not be reached through any other alias while
    ///   a method runs. This crate never synchronizes internally
    ///   (`alloc_pages`/`free_pages`/... take `&mut self`, the queries take
    ///   `&self`), so the caller must serialize access, typically by keeping
    ///   the allocator in a spin lock that also guards every other user of
    ///   the array.
    /// - The geometry rules of [`Buddy::new`] must hold for `nr_pages` and
    ///   `base`.
    ///
    /// # Errors
    ///
    /// Same as [`Buddy::new`], plus [`Error::InvalidPageArray`] if `ptr` is
    /// not aligned to `Page`.
    pub unsafe fn from_raw_parts(
        ptr: NonNull<Page>,
        nr_pages: usize,
        base: A,
        page_size: A,
    ) -> Result<Self, Error> {
        let mut buddy = Self::uninit();
        // SAFETY: the caller guarantees validity, lifetime and exclusive
        // access to the descriptor array.
        unsafe { buddy.set_state(ptr, nr_pages, base, page_size) }?;
        Ok(buddy)
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::vec::Vec;

    use super::*;

    const PS: usize = 0x1000;
    const PAGES: usize = 64;
    const MAX_ORDER: usize = 6;
    const MAX_BLOCK: usize = 32;

    fn arena(pages: &mut [Page]) -> Buddy<'_, usize, MAX_ORDER> {
        Buddy::new(0, PS, pages).unwrap()
    }

    fn freed_arena(pages: &mut [Page]) -> Buddy<'_, usize, MAX_ORDER> {
        let mut buddy = arena(pages);
        buddy.free_range(0, PAGES * PS).unwrap();
        buddy
    }

    #[test]
    fn uninit_is_inert_and_const() {
        // The placeholder must be usable in a constant initializer.
        const PLACEHOLDER: Buddy<'static, usize, MAX_ORDER> = Buddy::uninit();
        assert!(!PLACEHOLDER.is_initialized());
        assert_eq!(PLACEHOLDER.managed_pages(), 0);
        assert_eq!(PLACEHOLDER.nr_free(), 0);
        assert_eq!(PLACEHOLDER.base(), 0);
        assert_eq!(PLACEHOLDER.page_size(), 0);
        assert_eq!(PLACEHOLDER.nr_free_blocks(0), Some(0));

        // No operation may touch the dangling descriptor pointer.
        let mut buddy = Buddy::<usize, MAX_ORDER>::uninit();
        assert!(matches!(buddy.alloc_pages(0), Err(Error::Uninitialized)));
        assert!(matches!(
            buddy.free_pages(0, 0),
            Err(Error::Uninitialized)
        ));
        assert!(matches!(
            buddy.free_range(0, PAGES * PS),
            Err(Error::Uninitialized)
        ));
        assert!(matches!(buddy.validate(), Err(Error::Uninitialized)));
    }

    #[test]
    fn init_fills_the_placeholder_once() {
        let mut pages = [Page::EMPTY; PAGES];
        let ptr = NonNull::new(pages.as_mut_ptr()).unwrap();
        let mut buddy = Buddy::<usize, MAX_ORDER>::uninit();

        // SAFETY: `pages` is a local array, exclusively accessed through
        // `buddy`, and outlives it.
        unsafe { buddy.init(ptr, PAGES, 0, PS) }.unwrap();
        assert!(buddy.is_initialized());
        assert_eq!(buddy.managed_pages(), PAGES);
        assert_eq!(buddy.base(), 0);

        buddy.free_range(0, PAGES * PS).unwrap();
        assert_eq!(buddy.nr_free(), PAGES);
        buddy.validate().unwrap();

        // A second init is rejected before any descriptor is touched.
        // SAFETY: the same array; the call returns without touching it.
        assert!(matches!(
            unsafe { buddy.init(ptr, PAGES, 0, PS) },
            Err(Error::AlreadyInitialized)
        ));
        assert_eq!(buddy.nr_free(), PAGES);
        buddy.validate().unwrap();
    }

    #[test]
    fn failed_init_leaves_the_placeholder_intact() {
        let mut small = [Page::EMPTY; MAX_BLOCK - 1];
        let small_ptr = NonNull::new(small.as_mut_ptr()).unwrap();
        let mut buddy = Buddy::<usize, MAX_ORDER>::uninit();

        // SAFETY: the pointer is valid; the geometry check fails before the
        // descriptors are touched.
        assert!(matches!(
            unsafe { buddy.init(small_ptr, small.len(), 0, PS) },
            Err(Error::InvalidGeometry)
        ));
        assert!(!buddy.is_initialized());

        // A corrected init still succeeds afterwards.
        let mut pages = [Page::EMPTY; PAGES];
        let ptr = NonNull::new(pages.as_mut_ptr()).unwrap();
        // SAFETY: `pages` is a local array, exclusively accessed through
        // `buddy`, and outlives it.
        unsafe { buddy.init(ptr, PAGES, 0, PS) }.unwrap();
        buddy.free_range(0, PAGES * PS).unwrap();
        buddy.validate().unwrap();
    }

    #[test]
    fn new_rejects_zero_max_order() {
        let mut pages = [Page::EMPTY; 8];
        assert!(matches!(
            Buddy::<usize, 0>::new(0, PS, &mut pages),
            Err(Error::InvalidGeometry)
        ));
    }

    #[test]
    fn new_rejects_bad_page_size() {
        let mut pages = [Page::EMPTY; PAGES];
        assert!(matches!(
            Buddy::<usize, MAX_ORDER>::new(0, 0, &mut pages),
            Err(Error::InvalidPageSize)
        ));
        assert!(matches!(
            Buddy::<usize, MAX_ORDER>::new(0, 3, &mut pages),
            Err(Error::InvalidPageSize)
        ));
    }

    #[test]
    fn new_rejects_unaligned_base() {
        let mut pages = [Page::EMPTY; PAGES];
        // The largest block spans MAX_BLOCK pages; a base that is only page
        // aligned cannot express it.
        assert!(matches!(
            Buddy::<usize, MAX_ORDER>::new(PS, PS, &mut pages),
            Err(Error::InvalidGeometry)
        ));
    }

    #[test]
    fn new_rejects_arena_smaller_than_max_block() {
        let mut pages = [Page::EMPTY; MAX_BLOCK - 1];
        assert!(matches!(
            Buddy::<usize, MAX_ORDER>::new(0, PS, &mut pages),
            Err(Error::InvalidGeometry)
        ));
    }

    #[test]
    fn new_accepts_block_aligned_base() {
        let mut pages = [Page::EMPTY; PAGES];
        let base = MAX_BLOCK * PS;
        let buddy = Buddy::<usize, MAX_ORDER>::new(base, PS, &mut pages).unwrap();
        assert_eq!(buddy.base(), base);
        assert_eq!(buddy.page_size(), PS);
        assert_eq!(buddy.managed_pages(), PAGES);
        assert_eq!(buddy.nr_free(), 0);
        buddy.validate().unwrap();
    }

    #[test]
    fn free_range_bootstraps_and_coalesces() {
        let mut pages = [Page::EMPTY; PAGES];
        let buddy = freed_arena(&mut pages);

        assert_eq!(buddy.nr_free(), PAGES);
        // The largest two blocks cannot merge into an invalid order.
        assert_eq!(buddy.nr_free_blocks(5), Some(2));
        assert_eq!(buddy.nr_free_blocks(4), Some(0));
        assert_eq!(buddy.nr_free_blocks(MAX_ORDER as u8), None);
        buddy.validate().unwrap();
    }

    #[test]
    fn free_range_clips_to_managed_range() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        // Entirely outside: a no-op.
        buddy.free_range(PAGES * PS, 2 * PAGES * PS).unwrap();
        assert_eq!(buddy.nr_free(), 0);

        // Partially outside: only the overlap is freed.
        buddy.free_range(PS / 2, 3 * PS).unwrap();
        assert_eq!(buddy.nr_free(), 2);
        buddy.validate().unwrap();

        // Around the top of the arena: clipped to the last page.
        let mut buddy = arena(&mut pages);
        buddy.free_range((PAGES - 1) * PS, 2 * PAGES * PS).unwrap();
        assert_eq!(buddy.nr_free(), 1);
        buddy.validate().unwrap();
    }

    #[test]
    fn freeing_lower_pages_coalesces_existing_blocks() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        // A deliberately ragged range: [1, 31) splits into one block per
        // order, from 0 up to the largest that fits.
        buddy.free_range(PS, 31 * PS).unwrap();
        assert_eq!(buddy.nr_free(), 30);
        assert_eq!(buddy.nr_free_blocks(0), Some(2));
        assert_eq!(buddy.nr_free_blocks(1), Some(2));
        assert_eq!(buddy.nr_free_blocks(2), Some(2));
        assert_eq!(buddy.nr_free_blocks(3), Some(2));
        assert_eq!(buddy.nr_free_blocks(4), Some(0));
        buddy.validate().unwrap();

        // Freeing page 0 cascades merges through [0, 16).
        buddy.free_range(0, PS).unwrap();
        assert_eq!(buddy.nr_free(), 31);
        assert_eq!(buddy.nr_free_blocks(4), Some(1));
        assert_eq!(buddy.nr_free_blocks(3), Some(1));
        assert_eq!(buddy.nr_free_blocks(2), Some(1));
        assert_eq!(buddy.nr_free_blocks(1), Some(1));
        assert_eq!(buddy.nr_free_blocks(0), Some(1));
        buddy.validate().unwrap();
    }

    #[test]
    fn alloc_splits_from_the_largest_block() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        // The last freed block is the list head, so allocation starts at
        // the second large block.
        let addr = buddy.alloc_pages(2).unwrap();
        assert_eq!(addr, MAX_BLOCK * PS);
        assert_eq!(addr % (4 * PS), 0);
        assert_eq!(buddy.nr_free(), PAGES - 4);
        assert_eq!(buddy.nr_free_blocks(4), Some(1));
        assert_eq!(buddy.nr_free_blocks(3), Some(1));
        assert_eq!(buddy.nr_free_blocks(2), Some(1));

        buddy.free_pages(addr, 2).unwrap();
        assert_eq!(buddy.nr_free(), PAGES);
        assert_eq!(buddy.nr_free_blocks(5), Some(2));
        buddy.validate().unwrap();
    }

    #[test]
    fn alloc_and_free_every_page() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        let mut addrs = Vec::new();
        for _ in 0..PAGES {
            addrs.push(buddy.alloc_pages(0).unwrap());
        }
        assert!(matches!(buddy.alloc_pages(0), Err(Error::OutOfMemory)));
        assert_eq!(buddy.nr_free(), 0);

        addrs.sort_unstable();
        let expected: Vec<usize> = (0..PAGES).map(|idx| idx * PS).collect();
        assert_eq!(addrs, expected);

        for addr in addrs.into_iter().rev() {
            buddy.free_pages(addr, 0).unwrap();
        }
        assert_eq!(buddy.nr_free(), PAGES);
        assert_eq!(buddy.nr_free_blocks(5), Some(2));
        buddy.validate().unwrap();
    }

    #[test]
    fn alloc_rejects_invalid_order_and_reports_oom() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        assert!(matches!(
            buddy.alloc_pages(MAX_ORDER as u8),
            Err(Error::InvalidOrder)
        ));
        assert!(matches!(buddy.alloc_pages(0), Err(Error::OutOfMemory)));
    }

    #[test]
    fn free_pages_rejects_bad_input() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        assert!(matches!(
            buddy.free_pages(0, MAX_ORDER as u8),
            Err(Error::InvalidOrder)
        ));
        assert!(matches!(
            buddy.free_pages(PS / 2, 0),
            Err(Error::InvalidAddress)
        ));
        assert!(matches!(
            buddy.free_pages(PAGES * PS, 0),
            Err(Error::InvalidAddress)
        ));
        // A free head cannot be freed again.
        assert!(matches!(buddy.free_pages(0, 0), Err(Error::InvalidFree)));

        // An allocation cannot be freed at a misaligned address...
        let addr = buddy.alloc_pages(1).unwrap();
        assert!(matches!(
            buddy.free_pages(addr + PS, 1),
            Err(Error::InvalidAddress)
        ));
        // ...nor twice.
        buddy.free_pages(addr, 1).unwrap();
        assert!(matches!(
            buddy.free_pages(addr, 1),
            Err(Error::InvalidFree)
        ));
    }

    #[test]
    fn free_range_rejects_overlap_with_free_memory() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        buddy.free_range(0, 4 * PS).unwrap();
        // [1, 3) lies inside the free order-2 block [0, 4).
        assert!(matches!(
            buddy.free_range(PS, 2 * PS),
            Err(Error::InvalidFree)
        ));
        assert_eq!(buddy.nr_free(), 4);
        buddy.validate().unwrap();
    }

    #[test]
    fn free_range_rejects_overlap_with_allocated_memory() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        let addr = buddy.alloc_pages(0).unwrap();
        assert!(matches!(
            buddy.free_range(addr, addr + PS),
            Err(Error::InvalidFree)
        ));
        assert_eq!(buddy.nr_free(), PAGES - 1);
    }

    #[test]
    fn validate_detects_unlinked_free_head() {
        let mut pages = [Page::EMPTY; PAGES];
        let buddy = freed_arena(&mut pages);

        // Mark a page free without linking it into any list.
        {
            // SAFETY: the descriptor array is owned by this test.
            let pages = unsafe { pages_mut(buddy.pages, buddy.nr_pages) };
            pages[8].flags.insert(PageFlags::BUDDY);
            pages[8].order = 5;
        }
        assert!(matches!(
            buddy.validate(),
            Err(Error::CorruptFreeList)
        ));
    }

    #[test]
    fn validate_detects_uncoalesced_buddies() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        // Hand-build two free order-2 buddies that should have merged.
        {
            // SAFETY: the descriptor array is owned by this test.
            let pages = unsafe { pages_mut(buddy.pages, buddy.nr_pages) };
            for idx in [0usize, 4] {
                let page = &mut pages[idx];
                page.flags.insert(PageFlags::BUDDY);
                page.order = 2;
                buddy.areas[2].push_front(pages, idx);
            }
        }
        assert!(matches!(buddy.validate(), Err(Error::Uncoalesced)));
    }

    #[test]
    fn validate_detects_count_mismatch() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        buddy.nr_free = 0;
        assert!(matches!(buddy.validate(), Err(Error::CountMismatch)));
    }

    #[test]
    fn random_operations_keep_invariants() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        let mut state = 0x1234_5678_9abc_def0u64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut live: Vec<(usize, u8)> = Vec::new();
        for _ in 0..4000 {
            if rand() % 3 != 0 && !live.is_empty() {
                let i = (rand() as usize) % live.len();
                let (addr, order) = live.swap_remove(i);
                buddy.free_pages(addr, order).unwrap();
            } else {
                let order = (rand() % MAX_ORDER as u64) as u8;
                if let Ok(addr) = buddy.alloc_pages(order) {
                    live.push((addr, order));
                }
            }
            buddy.validate().unwrap();
        }

        // Every outstanding allocation must still be freeable, and the
        // arena must return to its fully free state.
        let mut allocated_pages = 0;
        for (addr, order) in live.drain(..) {
            allocated_pages += pages_in_order(order);
            buddy.free_pages(addr, order).unwrap();
        }
        assert!(allocated_pages <= PAGES);
        assert_eq!(buddy.nr_free(), PAGES);
        buddy.validate().unwrap();
    }

    // Interpretation is too slow for Miri at this size; the same code paths
    // are covered by `random_operations_keep_invariants`.
    #[cfg_attr(miri, ignore)]
    #[test]
    fn random_operations_on_kernel_sized_arena() {
        // MAX_ORDER 11 (orders 0..10) as configured in the kernel by
        // default, over a 8 MiB arena: 2048 pages of 4 KiB.
        const PAGES: usize = 2048;
        let mut pages = alloc::vec![Page::EMPTY; PAGES];
        let mut buddy = Buddy::<usize, 11>::new(0, PS, &mut pages).unwrap();
        buddy.free_range(0, PAGES * PS).unwrap();
        assert_eq!(buddy.nr_free(), PAGES);
        // 2048 pages = two 1024-page blocks at order 10.
        assert_eq!(buddy.nr_free_blocks(10), Some(2));

        let mut state = 0xdead_beef_cafe_babeu64;
        let mut rand = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut live: Vec<(usize, u8)> = Vec::new();
        for _ in 0..2000 {
            if rand() % 2 == 0 && !live.is_empty() {
                let i = (rand() as usize) % live.len();
                let (addr, order) = live.swap_remove(i);
                buddy.free_pages(addr, order).unwrap();
            } else {
                let order = (rand() % 11) as u8;
                if let Ok(addr) = buddy.alloc_pages(order) {
                    live.push((addr, order));
                }
            }
            buddy.validate().unwrap();
        }
    }
}
