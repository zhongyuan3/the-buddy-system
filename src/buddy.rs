//! The buddy allocator core.
//!
//! This module mirrors the data structures and algorithms of the kernel's
//! buddy allocator in `mm/page_alloc.c` and `include/linux/mmzone.h`:
//!
//! - [`Buddy::free_range`] mirrors `free_low_memory_core_early` /
//!   `__free_pages_memory` and bootstraps the allocator from memory
//!   discovered at boot (typically the free ranges of a memblock).
//! - `expand` (private) mirrors the kernel's `expand`.
//! - `free_one_page` (private) mirrors the kernel's `__free_one_page`.
//! - [`Buddy::alloc_pages`] mirrors `__rmqueue_smallest` plus `expand`.
//! - [`Buddy::free_pages`] mirrors `__free_pages` followed by
//!   `free_pages_prepare`'s state checks.

use core::cmp::max;
use core::cmp::min;
use core::marker::PhantomData;
use core::mem::align_of;
use core::ptr::NonNull;

use crate::PageFrame;
use crate::list::FreeList;
use crate::list::FreeListIter;
use crate::page::Page;
use crate::page::PageFlags;
use crate::page::pages_in_order;

/// Errors returned by the buddy allocator.
///
/// The variants mirror the failure modes the kernel's page allocator
/// asserts on (`bad_page`, `VM_BUG_ON_PAGE`, `check_new_page`) and the
/// `NULL`/`-ENOMEM` result of `__rmqueue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No free block of the requested order (or any larger order) exists.
    OutOfMemory,
    /// The requested order is above `MAX_ORDER` (`NR_PAGE_ORDERS - 1`).
    InvalidOrder,
    /// The address is misaligned, does not denote the start of a block, or
    /// lies outside the managed range.
    InvalidAddress,
    /// The block may not be freed: it is already free, in use, the freed
    /// range overlaps pages that are not free, or the order does not match
    /// the order the block was allocated with. Also reported when a free
    /// range overlaps an already freed region.
    InvalidFree,
    /// The page size is zero, not a power of two, or not representable.
    InvalidPageSize,
    /// The page descriptor array is unusable, for example because the
    /// pointer passed to [`Buddy::from_raw_parts`] is not aligned to
    /// [`Page`].
    InvalidPageArray,
    /// An operation that needs page descriptors was attempted on an
    /// allocator created by [`Buddy::uninit`] whose [`Buddy::init`] has not
    /// run yet.
    Uninitialized,
    /// [`Buddy::init`] was called on an allocator that already manages
    /// descriptors.
    AlreadyInitialized,
    /// The arena geometry is invalid: an empty descriptor array, a base
    /// address that is not page aligned, or an address range that would
    /// wrap around the top of the address space.
    InvalidGeometry,
    /// A free list is corrupted: a listed block is misaligned, not marked
    /// free, carries the wrong order, or is not linked where it is listed.
    CorruptFreeList,
    /// Two buddy blocks are free at the same order; coalescing failed.
    Uncoalesced,
    /// The free page accounting does not match the free lists.
    CountMismatch,
    /// An invariant was violated; indicates a bug in the implementation.
    InternalError,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Error::OutOfMemory => "Out of memory",
            Error::InvalidOrder => "Invalid order",
            Error::InvalidAddress => "Invalid address",
            Error::InvalidFree => "Invalid free",
            Error::InvalidPageSize => "Invalid page size",
            Error::InvalidPageArray => "Invalid page descriptor array",
            Error::InvalidGeometry => "Invalid geometry",
            Error::Uninitialized => "Allocator is not initialized",
            Error::AlreadyInitialized => "Allocator is already initialized",
            Error::CorruptFreeList => "Corrupted free list",
            Error::Uncoalesced => "Uncoalesced buddies",
            Error::CountMismatch => "Free page count mismatch",
            Error::InternalError => "Internal error",
        };
        write!(f, "{s}")
    }
}

impl core::error::Error for Error {}

/// The free list of one order, mirroring `struct free_area`
/// (`include/linux/mmzone.h`).
///
/// The kernel's `free_area` holds one list per migration type; migration
/// types are not implemented yet, so a single list is kept. Adding them
/// later only turns `free_list` into an array and requires a migration type
/// argument in the allocator, exactly like `migratetype` in
/// `__rmqueue_smallest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FreeArea {
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
    pub(crate) const fn nr_free(&self) -> usize {
        self.nr_free
    }

    /// Pushes a block head at the front of the list and bumps the count.
    pub(crate) fn push_front(&mut self, descriptors: &mut [Page], index: usize) {
        self.free_list.push_front(descriptors, index);
        self.nr_free += 1;
    }

    /// Unlinks a block head and drops the count.
    ///
    /// Clearing the `BUDDY` flag and the order mirrors the kernel's
    /// `del_page_from_free_list` (`__ClearPageBuddy` plus
    /// `set_page_private(page, 0)`).
    pub(crate) fn remove(&mut self, descriptors: &mut [Page], index: usize) {
        debug_assert!(descriptors[index].is_free());
        self.free_list.remove(descriptors, index);
        self.nr_free -= 1;
        clear_free_head(&mut descriptors[index]);
    }

    /// Pops the front block head of the given `order`, if any.
    pub(crate) fn pop_front(&mut self, descriptors: &mut [Page], order: usize) -> Option<usize> {
        let index = self.free_list.pop_front(descriptors)?;
        debug_assert!(descriptors[index].is_free());
        debug_assert_eq!(descriptors[index].order as usize, order);
        self.nr_free -= 1;
        clear_free_head(&mut descriptors[index]);
        Some(index)
    }

    /// Iterates the block heads of this order, front to back.
    pub(crate) fn iter<'p>(&self, descriptors: &'p [Page]) -> FreeListIter<'p> {
        self.free_list.iter(descriptors)
    }
}

/// Clears the free-head state of an unlinked block head.
fn clear_free_head(page: &mut Page) {
    page.flags.remove(PageFlags::BUDDY);
    page.order = 0;
}

/// A buddy allocator managing a contiguous range of physical page frames.
///
/// The allocator manages `nr_pages` page frames starting at `arena_base`. The
/// descriptors are supplied by the caller (the counterpart of the kernel's
/// `vmemmap`/`mem_map` array), so the allocator itself never allocates and
/// a kernel can place the array wherever its early boot code reserved room
/// for it.
///
/// `NR_PAGE_ORDERS` is the number of free areas, exactly like the kernel's
/// `NR_PAGE_ORDERS`: valid orders are `0..=MAX_ORDER`, where `MAX_ORDER`
/// is `NR_PAGE_ORDERS - 1` (the kernel's `MAX_PAGE_ORDER`), and the
/// largest block that can be allocated is `1 << MAX_ORDER` pages
/// (`MAX_ORDER_NR_PAGES`). The kernel's typical `MAX_PAGE_ORDER` of 10
/// therefore means `NR_PAGE_ORDERS == 11` and 4 MiB blocks on 4 KiB pages.
///
/// The const parameter is the array length itself because stable Rust
/// cannot express `[FreeArea; MAX_ORDER + 1]` with a generic `MAX_ORDER`.
/// At concrete call sites the kernel formula still works:
/// `const MAX_ORDER: usize = 10;` and `Buddy::<u64, { MAX_ORDER + 1 }>`.
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
/// offsets relative to `arena_base` are, through the checked
/// [`PageFrame::try_to_usize`]. Array indexing uses `usize` index space,
/// while buddy alignment and coalescing are computed in absolute PFN space,
/// so the arena base only needs page alignment.
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
/// let mut descriptors = [Page::EMPTY; 64];
/// let mut buddy = Buddy::<u64, 4>::new(0, 0x1000, &mut descriptors).unwrap();
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
pub struct Buddy<'a, A: PageFrame, const NR_PAGE_ORDERS: usize> {
    page_size: A,
    base_pfn: A,
    /// Address of the page descriptor array (the `vmemmap` counterpart).
    /// The slice is never stored; each method rebuilds it from this pointer
    /// through `pages_mut`/`pages_ref`.
    descriptors: NonNull<Page>,
    /// Number of page descriptors, i.e. the array length.
    nr_pages: usize,
    /// Whether a constructor or [`Buddy::init`] has supplied a descriptor
    /// array. Tracked explicitly instead of being derived from `nr_pages`,
    /// which is zero both before initialization and for the geometries the
    /// constructors reject.
    initialized: bool,
    areas: [FreeArea; NR_PAGE_ORDERS],
    nr_free: usize,
    /// Marker for the borrow held by [`Buddy::new`]; raw construction
    /// ([`Buddy::from_raw_parts`]) uses `'static`.
    _descriptors: PhantomData<&'a mut [Page]>,
}

// SAFETY: `Buddy` holds no thread-affine state, and the descriptor array is
// only dereferenced inside method calls. Every method's contract requires
// the caller to serialize access to the descriptors (see
// `from_raw_parts`), so moving the allocator between threads cannot create
// a data race.
unsafe impl<A: PageFrame + Send, const NR_PAGE_ORDERS: usize> Send
    for Buddy<'_, A, NR_PAGE_ORDERS>
{
}

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
/// `descriptors` must point to `nr_pages` initialized, properly aligned
/// `Page` descriptors that are not reached through any other alias while the
/// returned reference is alive.
unsafe fn pages_mut<'p>(descriptors: NonNull<Page>, nr_pages: usize) -> &'p mut [Page] {
    // SAFETY: the caller guarantees pointer validity and exclusivity.
    unsafe { core::slice::from_raw_parts_mut(descriptors.as_ptr(), nr_pages) }
}

/// Like [`pages_mut`], for read-only access.
///
/// # Safety
///
/// Same contract as [`pages_mut`]; the caller must additionally serialize
/// against writers.
unsafe fn pages_ref<'p>(descriptors: NonNull<Page>, nr_pages: usize) -> &'p [Page] {
    // SAFETY: the caller guarantees pointer validity.
    unsafe { core::slice::from_raw_parts(descriptors.as_ptr(), nr_pages) }
}

impl<'a, A: PageFrame, const NR_PAGE_ORDERS: usize> Buddy<'a, A, NR_PAGE_ORDERS> {
    /// The largest order the allocator can serve, i.e. `NR_PAGE_ORDERS - 1`.
    ///
    /// Mirrors the kernel's `MAX_PAGE_ORDER` (`MAX_ORDER_NR_PAGES` is
    /// `1 << MAX_ORDER`): valid orders are `0..=MAX_ORDER`, and the
    /// associated free area array has `NR_PAGE_ORDERS` entries.
    ///
    /// Saturating subtraction keeps the constant evaluable for the rejected
    /// `NR_PAGE_ORDERS == 0`; the constructors reject that geometry before
    /// the value is used.
    pub const MAX_ORDER: usize = NR_PAGE_ORDERS.saturating_sub(1);

    /// Creates an allocator over the page descriptor array `descriptors`.
    ///
    /// `arena_base` is the physical address of the first managed page and
    /// `page_size` its size. All descriptors are reset to [`Page::EMPTY`];
    /// no page is free until a range is handed to [`Buddy::free_range`] or
    /// [`Buddy::free_ranges`], mirroring the kernel, where memory is only
    /// freed into the buddy allocator by `memblock_free_all`.
    ///
    /// The borrow guarantees that the descriptor array outlives the
    /// allocator; kernel code that cannot name such a borrow uses
    /// [`Buddy::from_raw_parts`] instead.
    ///
    /// The arena must satisfy the invariants the kernel guarantees for
    /// zones:
    ///
    /// - the descriptor array holds at least one page;
    /// - its length survives a `from_usize`/`try_to_usize` round trip, so
    ///   every index is exact;
    /// - `arena_base` is page aligned;
    /// - the managed range does not wrap around the top of the address
    ///   space of `A`.
    ///
    /// The arena no longer has to be as large as the largest block or
    /// aligned to it: buddy alignment is computed in absolute PFN space, so
    /// small and page-aligned-but-unaligned arenas are supported.
    ///
    /// Mirrors `free_area_init` and `zone_init_free_lists` for one zone.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidPageSize`] if `page_size` is zero, not a
    /// power of two, or not representable as `usize`;
    /// [`Error::InvalidGeometry`] if any of the geometry invariants above
    /// is violated.
    pub fn new(arena_base: A, page_size: A, descriptors: &'a mut [Page]) -> Result<Self, Error> {
        let nr_pages = descriptors.len();
        if nr_pages == 0 {
            return Err(Error::InvalidGeometry);
        }
        let descriptors_ptr =
            NonNull::new(descriptors.as_mut_ptr()).expect("slice pointers are never null");

        let mut buddy = Self::uninit();
        // SAFETY: `descriptors` is a valid, exclusively borrowed slice for
        // `'a`, and the returned allocator holds that borrow through
        // `PhantomData`.
        unsafe { buddy.set_state(descriptors_ptr, nr_pages, arena_base, page_size) }?;
        Ok(buddy)
    }

    /// A `const` placeholder for static definitions.
    ///
    /// The placeholder manages zero descriptors. Until [`Buddy::init`] runs, the
    /// queries report zeroes and the operations that need descriptors
    /// (`alloc_pages`, `free_pages`, `free_range`, `validate`) report
    /// [`Error::Uninitialized`]; the dangling internal pointer is never
    /// dereferenced. This makes `Buddy::uninit()` sound as the initializer
    /// of a static that is filled in at boot.
    pub const fn uninit() -> Self {
        Self {
            page_size: A::ZERO,
            base_pfn: A::ZERO,
            descriptors: NonNull::dangling(),
            nr_pages: 0,
            initialized: false,
            areas: [FreeArea::new(); NR_PAGE_ORDERS],
            nr_free: 0,
            _descriptors: PhantomData,
        }
    }

    /// Returns `true` once a constructor or [`Buddy::init`] has supplied a
    /// descriptor array.
    pub const fn is_initialized(&self) -> bool {
        self.initialized
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
    /// Same contract as [`Buddy::from_raw_parts`]: `descriptors` must point
    /// to `nr_pages` properly aligned `Page` descriptors that stay valid and
    /// exclusively accessible (under the caller's synchronization) for as
    /// long as the allocator is used.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyInitialized`] if the allocator already manages
    /// descriptors; otherwise the same errors as [`Buddy::new`].
    pub unsafe fn init(
        &mut self,
        descriptors: NonNull<Page>,
        nr_pages: usize,
        arena_base: A,
        page_size: A,
    ) -> Result<(), Error> {
        if self.is_initialized() {
            return Err(Error::AlreadyInitialized);
        }
        // SAFETY: the caller guarantees the descriptor contract.
        unsafe { self.set_state(descriptors, nr_pages, arena_base, page_size) }
    }

    /// Shared body of every raw initialization: validates the geometry and
    /// resets the descriptors to [`Page::EMPTY`].
    ///
    /// # Safety
    ///
    /// `descriptors` must point to `nr_pages` valid, properly aligned `Page`
    /// descriptors that stay exclusively accessible for the whole call and
    /// for every later method call; see [`Buddy::from_raw_parts`].
    unsafe fn set_state(
        &mut self,
        descriptors: NonNull<Page>,
        nr_pages: usize,
        arena_base: A,
        page_size: A,
    ) -> Result<(), Error> {
        if NR_PAGE_ORDERS == 0 || NR_PAGE_ORDERS > usize::BITS as usize {
            return Err(Error::InvalidGeometry);
        }
        if (descriptors.as_ptr() as usize) % align_of::<Page>() != 0 {
            return Err(Error::InvalidPageArray);
        }

        let page_size_usize = page_size.try_to_usize().ok_or(Error::InvalidPageSize)?;
        if page_size_usize == 0 || !page_size_usize.is_power_of_two() {
            return Err(Error::InvalidPageSize);
        }
        if nr_pages == 0 {
            return Err(Error::InvalidGeometry);
        }

        // The page count must be representable as an index and as a PFN:
        // this rejects narrow address types whose `from_usize` saturates.
        let len = A::try_from_usize(nr_pages).ok_or(Error::InvalidGeometry)?;

        // `arena_base` must be page aligned; buddy alignment itself is
        // computed in absolute PFN space, so it does not need the
        // largest-block alignment the earlier implementation required.
        if A::align_down(arena_base, page_size) != arena_base {
            return Err(Error::InvalidGeometry);
        }
        let base_pfn = A::phys_to_pfn(arena_base, page_size);

        // The managed range may not wrap around the top of the address
        // space, and the last page's address must be representable.
        if base_pfn > A::MAX - len {
            return Err(Error::InvalidGeometry);
        }
        let max_pfn = A::phys_to_pfn(A::MAX, page_size);
        let last = len - A::from_usize(1);
        if base_pfn > max_pfn || max_pfn - base_pfn < last {
            return Err(Error::InvalidGeometry);
        }

        // SAFETY: the caller guarantees validity and exclusivity.
        for page in unsafe { pages_mut(descriptors, nr_pages) }.iter_mut() {
            *page = Page::EMPTY;
        }

        self.page_size = page_size;
        self.base_pfn = base_pfn;
        self.descriptors = descriptors;
        self.nr_pages = nr_pages;
        self.areas = [FreeArea::new(); NR_PAGE_ORDERS];
        self.nr_free = 0;
        self.initialized = true;
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
    /// (`zone->free_area[order].nr_free`), or `None` if `order` is above
    /// `MAX_ORDER`.
    pub fn nr_free_blocks(&self, order: u8) -> Option<usize> {
        if (order as usize) < NR_PAGE_ORDERS {
            Some(self.areas[order as usize].nr_free())
        } else {
            None
        }
    }

    /// Converts a page frame number to an index into the descriptor array.
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
        let index = (pfn - self.base_pfn)
            .try_to_usize()
            .ok_or(Error::InvalidAddress)?;
        if index >= self.nr_pages {
            return Err(Error::InvalidAddress);
        }
        Ok(index)
    }

    /// Converts an index into a page frame number. `index` may be
    /// `nr_pages` (one past the last page).
    fn pfn_of_index(&self, index: usize) -> A {
        debug_assert!(index <= self.nr_pages);
        self.base_pfn + A::from_usize(index)
    }

    /// Converts an index into a physical address (`PFN_PHYS`).
    fn addr_of_index(&self, index: usize) -> A {
        A::pfn_to_phys(self.pfn_of_index(index), self.page_size)
    }

    /// Converts a physical address of a page start into an index.
    fn index_of_addr(&self, page_addr: A) -> Result<usize, Error> {
        if A::align_down(page_addr, self.page_size) != page_addr {
            return Err(Error::InvalidAddress);
        }
        self.index_of_pfn(A::phys_to_pfn(page_addr, self.page_size))
    }

    /// Returns `(buddy_index, merged_index)` for the block at `index` of `order`,
    /// using absolute PFN alignment.
    ///
    /// Returns `None` when the buddy lies outside the arena. The arena base
    /// does not need to be aligned to the block size; alignment is computed
    /// from the absolute PFN, so small and unaligned arenas are supported.
    fn buddy_indices(&self, index: usize, order: u8) -> Option<(usize, usize)> {
        let pfn = self.pfn_of_index(index);
        let block_pfn = A::from_usize(pages_in_order(order));

        // Buddy pairs are aligned to `2 * block_pfn`. Compute the pair start
        // in absolute PFN space; the arena base does not need to be aligned.
        // When `2 * block_pfn` would overflow the address type, the only
        // possible pair start is zero.
        let pair_start = if block_pfn > A::MAX - block_pfn {
            A::ZERO
        } else {
            A::align_down(pfn, block_pfn + block_pfn)
        };
        let offset = pfn - pair_start;

        let (merged_pfn, buddy_pfn) = if offset < block_pfn {
            // Lower half of the pair: the buddy starts one block later.
            if pair_start > A::MAX - block_pfn {
                return None;
            }
            (pair_start, pair_start + block_pfn)
        } else {
            // Upper half: the buddy is the lower half at the pair start.
            (pair_start, pair_start)
        };

        let buddy_index = self.index_of_pfn(buddy_pfn).ok()?;
        let merged_index = self.index_of_pfn(merged_pfn).ok()?;
        Some((buddy_index, merged_index))
    }

    /// Returns `true` if `index` lies inside a free block.
    ///
    /// A page is covered by a free block of order `k` if and only if the
    /// absolute PFN rounded down to `1 << k` pages is a free head of order
    /// `k`. Used to reject ranges that partially overlap memory that is
    /// already free.
    ///
    /// Takes the descriptor slice rather than `&self` so that callers
    /// already holding it do not construct a second, aliasing one.
    fn is_inside_free_block(&self, descriptors: &[Page], index: usize) -> bool {
        let pfn = self.pfn_of_index(index);
        let max_order = (self.nr_pages.ilog2() as usize).min(Self::MAX_ORDER);

        for order in 0..=max_order {
            let block_pfn = A::from_usize(pages_in_order(order as u8));
            let head_pfn = A::align_down(pfn, block_pfn);
            let Ok(head_index) = self.index_of_pfn(head_pfn) else {
                continue;
            };
            let page = &descriptors[head_index];
            if page.is_free() && page.order as usize == order {
                return true;
            }
        }
        false
    }

    /// Splits the block at `index` down to `low`, adding the upper half of
    /// each intermediate order to the free lists.
    ///
    /// Mirrors the kernel's `expand`. Takes the descriptor slice instead of
    /// rebuilding it, so that callers holding one never create a second,
    /// aliasing slice (Stacked Borrows forbids using the older one
    /// afterwards).
    fn expand(
        &mut self,
        descriptors: &mut [Page],
        index: usize,
        low_order: u8,
        mut high_order: u8,
    ) {
        debug_assert!(high_order >= low_order);

        while high_order > low_order {
            high_order -= 1;
            let half = pages_in_order(high_order);
            let upper_index = index + half;

            // TODO(guard): let `set_page_guard` consume a split block for
            // CONFIG_DEBUG_PAGEALLOC/hugetlb instead of freeing it.
            let page = &mut descriptors[upper_index];
            debug_assert!(!page.is_free());
            page.flags.insert(PageFlags::BUDDY);
            page.order = high_order;

            self.areas[high_order as usize].push_front(descriptors, upper_index);
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
    /// Returns [`Error::InvalidOrder`] if `order > MAX_ORDER`, or
    /// [`Error::OutOfMemory`] if no free block of that order or larger
    /// exists.
    pub fn alloc_pages(&mut self, order: u8) -> Result<A, Error> {
        if !self.is_initialized() {
            return Err(Error::Uninitialized);
        }
        if order as usize >= NR_PAGE_ORDERS {
            return Err(Error::InvalidOrder);
        }

        // SAFETY: `&mut self`; access to the descriptors is externally
        // synchronized, see `from_raw_parts`. The slice is built once and
        // passed down, never rebuilt while an older one is still in use.
        let descriptors = unsafe { pages_mut(self.descriptors, self.nr_pages) };

        // TODO(migratetype): serve the requested migration type's list
        // first and fall back to the other lists when it is empty,
        // mirroring `__rmqueue_fallback`.
        for current_order in order as usize..NR_PAGE_ORDERS {
            let Some(index) = self.areas[current_order].pop_front(descriptors, current_order)
            else {
                continue;
            };

            self.nr_free -= pages_in_order(current_order as u8);
            self.expand(descriptors, index, order, current_order as u8);

            let page = &mut descriptors[index];
            page.flags.remove(PageFlags::BUDDY);
            page.order = order;
            page.refcount = 1;
            return Ok(self.addr_of_index(index));
        }

        Err(Error::OutOfMemory)
    }

    /// Puts the block of `2^order` pages starting at `block_start` back on
    /// the free lists, coalescing it with free buddy blocks.
    ///
    /// The block must have been returned by [`Buddy::alloc_pages`] with the
    /// same order. Mirrors `__free_pages` (`put_page_testzero` plus
    /// `free_pages_prepare`'s state checks).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOrder`] if `order > MAX_ORDER`.
    /// [`Error::InvalidAddress`] if `block_start` is not page aligned,
    /// denotes a block head that is misaligned for its size, or lies outside
    /// the managed range. [`Error::InvalidFree`] if the block is already
    /// free, overlaps free memory, or is not in use.
    pub fn free_pages(&mut self, block_start: A, order: u8) -> Result<(), Error> {
        if !self.is_initialized() {
            return Err(Error::Uninitialized);
        }
        if order as usize >= NR_PAGE_ORDERS {
            return Err(Error::InvalidOrder);
        }

        let index = self.index_of_addr(block_start)?;
        let block = pages_in_order(order);
        let end = index.checked_add(block).ok_or(Error::InvalidAddress)?;
        if end > self.nr_pages {
            return Err(Error::InvalidAddress);
        }
        // The block must be naturally aligned in absolute PFN space, not
        // merely in index space.
        let pfn = A::phys_to_pfn(block_start, self.page_size);
        let block_pfn = A::from_usize(block);
        if A::align_down(pfn, block_pfn) != pfn {
            return Err(Error::InvalidAddress);
        }

        // SAFETY: `&mut self`; access to the descriptors is externally
        // synchronized, see `from_raw_parts`.
        let descriptors = unsafe { pages_mut(self.descriptors, self.nr_pages) };
        let page = &mut descriptors[index];
        if page.is_free() || page.refcount != 1 {
            return Err(Error::InvalidFree);
        }
        if page.order != order {
            return Err(Error::InvalidFree);
        }
        page.refcount = 0;

        self.free_one_page(descriptors, index, order)
    }

    /// Frees `[start, end)`, splitting it into the largest naturally
    /// aligned blocks. Parts outside the managed range are ignored, which
    /// makes it safe to feed whole memory ranges discovered at boot.
    ///
    /// Mirrors `free_low_memory_core_early` and its helper
    /// `__free_pages_memory`: the chunk order is limited by the absolute
    /// PFN's alignment (`__ffs(start)pfn`) and by the remaining length.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFree`] if the range overlaps memory that is
    /// already free or in use.
    pub fn free_range(&mut self, start: A, end: A) -> Result<(), Error> {
        if !self.is_initialized() {
            return Err(Error::Uninitialized);
        }

        let start_pfn = max(A::pfn_ceil(start, self.page_size), self.base_pfn);
        let end_limit = self.pfn_of_index(self.nr_pages);
        let end_pfn = min(A::phys_to_pfn(end, self.page_size), end_limit);

        if start_pfn >= end_pfn {
            return Ok(());
        }

        let mut index = self.index_of_pfn(start_pfn)?;
        let end_index = if end_pfn == end_limit {
            self.nr_pages
        } else {
            self.index_of_pfn(end_pfn)?
        };

        // SAFETY: `&mut self`; access to the descriptors is externally
        // synchronized, see `from_raw_parts`.
        let descriptors = unsafe { pages_mut(self.descriptors, self.nr_pages) };

        while index < end_index {
            let remaining = end_index - index;
            let pfn = self.pfn_of_index(index);
            let mut order = 0u32;
            let max_order = Self::MAX_ORDER as u32;
            while order < max_order {
                let next = pages_in_order((order + 1) as u8);
                if next > remaining || A::align_down(pfn, A::from_usize(next)) != pfn {
                    break;
                }
                order += 1;
            }
            debug_assert!(pages_in_order(order as u8) <= remaining);

            self.free_one_page(descriptors, index, order as u8)?;
            index += pages_in_order(order as u8);
        }

        Ok(())
    }

    /// Frees each `[start, end)` range in the iterator, exactly like
    /// calling [`Buddy::free_range`] for every range in turn.
    ///
    /// Useful for callers that hold a list of ranges instead of a live
    /// source, for example a snapshot of a boot allocator's free ranges
    /// taken while the boot allocator's lock was held, so that the lock can
    /// be released before the allocator is touched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFree`] if any range overlaps memory that is
    /// already free or in use.
    ///
    /// # Examples
    ///
    /// ```
    /// use the_buddy_system::Buddy;
    /// use the_buddy_system::Page;
    ///
    /// let mut descriptors = [Page::EMPTY; 64];
    /// let mut buddy = Buddy::<usize, 4>::new(0, 0x1000, &mut descriptors).unwrap();
    ///
    /// buddy.free_ranges([(0usize, 0x10_000)]).unwrap();
    /// assert_eq!(buddy.nr_free(), 16);
    /// ```
    pub fn free_ranges<I>(&mut self, ranges: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = (A, A)>,
    {
        for (start, end) in ranges {
            self.free_range(start, end)?;
        }
        Ok(())
    }

    /// Adds the block at `index` to the free lists, merging with free
    /// buddies until no larger block can be formed.
    ///
    /// Mirrors the kernel's `__free_one_page`. The caller must have
    /// verified that the block is fully free-able.
    fn free_one_page(
        &mut self,
        descriptors: &mut [Page],
        index: usize,
        order: u8,
    ) -> Result<(), Error> {
        debug_assert!(
            A::align_down(
                self.pfn_of_index(index),
                A::from_usize(pages_in_order(order))
            ) == self.pfn_of_index(index)
        );

        if self.is_inside_free_block(descriptors, index) {
            return Err(Error::InvalidFree);
        }
        if descriptors[index].refcount != 0 {
            return Err(Error::InvalidFree);
        }

        let mut index = index;
        let mut order = order;
        while (order as usize) < Self::MAX_ORDER {
            // TODO(bitmap): consult the per-order buddy bitmap first, as
            // `buddy_merge_likely` does, to avoid touching a cache-cold
            // buddy page for every failed merge.
            let Some((buddy_index, merged_index)) = self.buddy_indices(index, order) else {
                break;
            };
            let page = &descriptors[buddy_index];
            if !page.is_free() || page.order != order {
                break;
            }

            self.areas[order as usize].remove(descriptors, buddy_index);
            self.nr_free -= pages_in_order(order);
            index = merged_index;
            order += 1;
        }

        // TODO(antifrag): insert at the tail when the merged block's
        // buddy is likely free, mirroring `buddy_merge_likely` and
        // `add_to_free_list_tail`; this keeps large blocks mergeable and
        // reduces fragmentation.
        let page = &mut descriptors[index];
        page.flags.insert(PageFlags::BUDDY);
        page.order = order;
        self.areas[order as usize].push_front(descriptors, index);
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
        let descriptors = unsafe { pages_ref(self.descriptors, self.nr_pages) };

        for order in 0..NR_PAGE_ORDERS {
            let block = pages_in_order(order as u8);
            let mut count = 0usize;

            for index in self.areas[order].iter(descriptors) {
                count += 1;
                // A cycle in a corrupted list would otherwise loop forever.
                if count > self.nr_pages {
                    return Err(Error::CorruptFreeList);
                }
                if block > self.nr_pages || index > self.nr_pages - block {
                    return Err(Error::CorruptFreeList);
                }
                let pfn = self.pfn_of_index(index);
                let block_pfn = A::from_usize(block);
                if A::align_down(pfn, block_pfn) != pfn {
                    return Err(Error::CorruptFreeList);
                }
                let page = &descriptors[index];
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
            if order + 1 < NR_PAGE_ORDERS {
                for index in self.areas[order].iter(descriptors) {
                    if let Some((buddy_index, _)) = self.buddy_indices(index, order as u8) {
                        let page = &descriptors[buddy_index];
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

        let flagged = descriptors.iter().filter(|page| page.is_free()).count();
        if flagged != listed_heads {
            return Err(Error::CorruptFreeList);
        }

        Ok(())
    }
}

impl<A: PageFrame, const NR_PAGE_ORDERS: usize> Buddy<'static, A, NR_PAGE_ORDERS> {
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
    /// - `descriptors` must point to `nr_pages` properly aligned `Page`
    ///   descriptors that stay valid for reads and writes for as long as the
    ///   allocator (or anything derived from it) is used; the kernel's
    ///   `vmemmap` satisfies this by construction.
    /// - The descriptors must not be reached through any other alias while
    ///   a method runs. This crate never synchronizes internally
    ///   (`alloc_pages`/`free_pages`/... take `&mut self`, the queries take
    ///   `&self`), so the caller must serialize access, typically by keeping
    ///   the allocator in a spin lock that also guards every other user of
    ///   the array.
    /// - The geometry rules of [`Buddy::new`] must hold for `nr_pages` and
    ///   `arena_base`.
    ///
    /// # Errors
    ///
    /// Same as [`Buddy::new`], plus [`Error::InvalidPageArray`] if
    /// `descriptors` is not aligned to `Page`.
    pub unsafe fn from_raw_parts(
        descriptors: NonNull<Page>,
        nr_pages: usize,
        arena_base: A,
        page_size: A,
    ) -> Result<Self, Error> {
        let mut buddy = Self::uninit();
        // SAFETY: the caller guarantees validity, lifetime and exclusive
        // access to the descriptor array.
        unsafe { buddy.set_state(descriptors, nr_pages, arena_base, page_size) }?;
        Ok(buddy)
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::string::ToString;
    use alloc::vec::Vec;

    use super::*;

    const PS: usize = 0x1000;
    const PAGES: usize = 64;
    const NR_PAGE_ORDERS: usize = 6;
    const MAX_ORDER: usize = NR_PAGE_ORDERS - 1;
    const MAX_BLOCK: usize = 32;

    fn arena(pages: &mut [Page]) -> Buddy<'_, usize, NR_PAGE_ORDERS> {
        Buddy::new(0, PS, pages).unwrap()
    }

    fn freed_arena(pages: &mut [Page]) -> Buddy<'_, usize, NR_PAGE_ORDERS> {
        let mut buddy = arena(pages);
        buddy.free_range(0, PAGES * PS).unwrap();
        buddy
    }

    #[test]
    fn uninit_is_inert_and_const() {
        // The placeholder must be usable in a constant initializer.
        const PLACEHOLDER: Buddy<'static, usize, NR_PAGE_ORDERS> = Buddy::uninit();
        assert!(!PLACEHOLDER.is_initialized());
        assert_eq!(PLACEHOLDER.managed_pages(), 0);
        assert_eq!(PLACEHOLDER.nr_free(), 0);
        assert_eq!(PLACEHOLDER.base(), 0);
        assert_eq!(PLACEHOLDER.page_size(), 0);
        assert_eq!(PLACEHOLDER.nr_free_blocks(0), Some(0));

        // No operation may touch the dangling descriptor pointer.
        let mut buddy = Buddy::<usize, NR_PAGE_ORDERS>::uninit();
        assert!(matches!(buddy.alloc_pages(0), Err(Error::Uninitialized)));
        assert!(matches!(buddy.free_pages(0, 0), Err(Error::Uninitialized)));
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
        let mut buddy = Buddy::<usize, NR_PAGE_ORDERS>::uninit();

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
        let mut buddy = Buddy::<usize, NR_PAGE_ORDERS>::uninit();

        // SAFETY: the pointer is valid; the geometry check fails before the
        // descriptors are touched.
        assert!(matches!(
            unsafe { buddy.init(small_ptr, small.len(), PS / 2, PS) },
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
    fn new_rejects_zero_nr_page_orders() {
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
            Buddy::<usize, NR_PAGE_ORDERS>::new(0, 0, &mut pages),
            Err(Error::InvalidPageSize)
        ));
        assert!(matches!(
            Buddy::<usize, NR_PAGE_ORDERS>::new(0, 3, &mut pages),
            Err(Error::InvalidPageSize)
        ));
    }

    #[test]
    fn new_accepts_page_aligned_base() {
        let mut pages = [Page::EMPTY; PAGES];
        // Base is page aligned but not aligned to the largest block; buddy
        // alignment is computed in absolute PFN space.
        let mut buddy = Buddy::<usize, NR_PAGE_ORDERS>::new(PS, PS, &mut pages).unwrap();
        assert_eq!(buddy.base(), PS);
        buddy.free_range(PS, PS + PAGES * PS).unwrap();
        assert_eq!(buddy.nr_free(), PAGES);

        let addr = buddy.alloc_pages(2).unwrap();
        assert_eq!(addr % (4 * PS), 0);
        buddy.free_pages(addr, 2).unwrap();
        buddy.validate().unwrap();
    }

    #[test]
    fn unaligned_base_allocates_and_coalesces_all_pages() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = Buddy::<usize, NR_PAGE_ORDERS>::new(PS, PS, &mut pages).unwrap();
        buddy.free_range(PS, PS + PAGES * PS).unwrap();

        let mut live = Vec::new();
        while let Ok(addr) = buddy.alloc_pages(0) {
            live.push(addr);
        }
        assert_eq!(live.len(), PAGES);

        for addr in live {
            buddy.free_pages(addr, 0).unwrap();
        }
        assert_eq!(buddy.nr_free(), PAGES);
        buddy.validate().unwrap();
    }

    #[test]
    fn new_accepts_arena_smaller_than_max_block() {
        let mut pages = [Page::EMPTY; MAX_BLOCK - 1];
        let mut buddy = Buddy::<usize, NR_PAGE_ORDERS>::new(0, PS, &mut pages).unwrap();
        buddy.free_range(0, (MAX_BLOCK - 1) * PS).unwrap();
        assert_eq!(buddy.nr_free(), MAX_BLOCK - 1);
        // 31 pages split into 16 + 8 + 4 + 2 + 1.
        assert_eq!(buddy.nr_free_blocks(4), Some(1));
        assert_eq!(buddy.nr_free_blocks(5), Some(0));

        let addr = buddy.alloc_pages(3).unwrap();
        buddy.free_pages(addr, 3).unwrap();
        buddy.validate().unwrap();
    }

    #[test]
    fn new_rejects_non_page_aligned_base() {
        let mut pages = [Page::EMPTY; PAGES];
        assert!(matches!(
            Buddy::<usize, NR_PAGE_ORDERS>::new(PS / 2, PS, &mut pages),
            Err(Error::InvalidGeometry)
        ));
    }

    #[test]
    fn new_accepts_block_aligned_base() {
        let mut pages = [Page::EMPTY; PAGES];
        let base = MAX_BLOCK * PS;
        let buddy = Buddy::<usize, NR_PAGE_ORDERS>::new(base, PS, &mut pages).unwrap();
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
        assert_eq!(buddy.nr_free_blocks(MAX_ORDER as u8), Some(2));
        assert_eq!(buddy.nr_free_blocks(4), Some(0));
        assert_eq!(buddy.nr_free_blocks(NR_PAGE_ORDERS as u8), None);
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
    fn free_ranges_feeds_multiple_ranges() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        buddy.free_ranges([(0usize, PS), (2 * PS, 4 * PS)]).unwrap();
        assert_eq!(buddy.nr_free(), 3);
        assert_eq!(buddy.nr_free_blocks(0), Some(1));
        assert_eq!(buddy.nr_free_blocks(1), Some(1));
        buddy.validate().unwrap();

        // An empty iterator is a no-op.
        buddy
            .free_ranges(core::iter::empty::<(usize, usize)>())
            .unwrap();
        assert_eq!(buddy.nr_free(), 3);
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
        assert_eq!(buddy.nr_free_blocks(MAX_ORDER as u8), Some(2));
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
        let expected: Vec<usize> = (0..PAGES).map(|index| index * PS).collect();
        assert_eq!(addrs, expected);

        for addr in addrs.into_iter().rev() {
            buddy.free_pages(addr, 0).unwrap();
        }
        assert_eq!(buddy.nr_free(), PAGES);
        assert_eq!(buddy.nr_free_blocks(MAX_ORDER as u8), Some(2));
        buddy.validate().unwrap();
    }

    #[test]
    fn alloc_rejects_invalid_order_and_reports_oom() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        assert!(matches!(
            buddy.alloc_pages(NR_PAGE_ORDERS as u8),
            Err(Error::InvalidOrder)
        ));
        assert!(matches!(buddy.alloc_pages(0), Err(Error::OutOfMemory)));
    }

    #[test]
    fn free_pages_rejects_bad_input() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        assert!(matches!(
            buddy.free_pages(0, NR_PAGE_ORDERS as u8),
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
        assert!(matches!(buddy.free_pages(addr, 1), Err(Error::InvalidFree)));
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
            let pages = unsafe { pages_mut(buddy.descriptors, buddy.nr_pages) };
            pages[8].flags.insert(PageFlags::BUDDY);
            pages[8].order = 5;
        }
        assert!(matches!(buddy.validate(), Err(Error::CorruptFreeList)));
    }

    #[test]
    fn validate_detects_uncoalesced_buddies() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = arena(&mut pages);

        // Hand-build two free order-2 buddies that should have merged.
        {
            // SAFETY: the descriptor array is owned by this test.
            let pages = unsafe { pages_mut(buddy.descriptors, buddy.nr_pages) };
            for index in [0usize, 4] {
                let page = &mut pages[index];
                page.flags.insert(PageFlags::BUDDY);
                page.order = 2;
                buddy.areas[2].push_front(pages, index);
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
                let order = (rand() % NR_PAGE_ORDERS as u64) as u8;
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
        // NR_PAGE_ORDERS 11 (MAX_ORDER 10, orders 0..=10) as configured in
        // the kernel by default, over a 8 MiB arena: 2048 pages of 4 KiB.
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

    #[test]
    fn free_rejects_a_mismatched_order() {
        let mut pages = [Page::EMPTY; PAGES];
        let mut buddy = freed_arena(&mut pages);

        let addr = buddy.alloc_pages(2).unwrap();
        assert_eq!(buddy.nr_free(), PAGES - 4);

        // Freeing the same block with the wrong order must not touch the
        // free lists or the descriptor state.
        assert!(matches!(buddy.free_pages(addr, 1), Err(Error::InvalidFree)));
        assert_eq!(buddy.nr_free(), PAGES - 4);
        buddy.validate().unwrap();

        buddy.free_pages(addr, 2).unwrap();
        assert_eq!(buddy.nr_free(), PAGES);
        buddy.validate().unwrap();
    }

    #[test]
    fn error_is_copy_and_eq() {
        let e = Error::OutOfMemory;
        let copy = e;
        assert_eq!(e, copy);
        assert_ne!(Error::OutOfMemory, Error::InvalidOrder);
    }

    #[test]
    fn display_covers_all_variants() {
        assert_eq!(Error::OutOfMemory.to_string().as_str(), "Out of memory");
        assert_eq!(Error::InvalidOrder.to_string().as_str(), "Invalid order");
        assert_eq!(
            Error::InvalidAddress.to_string().as_str(),
            "Invalid address"
        );
        assert_eq!(Error::InvalidFree.to_string().as_str(), "Invalid free");
        assert_eq!(
            Error::InvalidPageSize.to_string().as_str(),
            "Invalid page size"
        );
        assert_eq!(
            Error::InvalidPageArray.to_string().as_str(),
            "Invalid page descriptor array"
        );
        assert_eq!(
            Error::InvalidGeometry.to_string().as_str(),
            "Invalid geometry"
        );
        assert_eq!(
            Error::Uninitialized.to_string().as_str(),
            "Allocator is not initialized"
        );
        assert_eq!(
            Error::AlreadyInitialized.to_string().as_str(),
            "Allocator is already initialized"
        );
        assert_eq!(
            Error::CorruptFreeList.to_string().as_str(),
            "Corrupted free list"
        );
        assert_eq!(
            Error::Uncoalesced.to_string().as_str(),
            "Uncoalesced buddies"
        );
        assert_eq!(
            Error::CountMismatch.to_string().as_str(),
            "Free page count mismatch"
        );
        assert_eq!(Error::InternalError.to_string().as_str(), "Internal error");
    }
}
