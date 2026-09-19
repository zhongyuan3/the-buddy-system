//! Boot time integration with [`the_memblock`].
//!
//! Mirrors the kernel's handoff from the early boot allocator to the page
//! allocator: `memblock_free_all` walks the free memory ranges left in
//! memblock and frees them into the buddy allocator's zones
//! (`free_low_memory_core_early`). After that, memblock is no longer used.

use the_memblock::flags::MemblockFlags;
use the_memblock::memblock::Memblock;

use crate::buddy::Buddy;
use crate::error::Error;
use crate::pfn::PageFrame;

impl<A: PageFrame, const NR_PAGE_ORDERS: usize> Buddy<'_, A, NR_PAGE_ORDERS> {
    /// Frees every free memory range tracked by `mb` into the allocator.
    ///
    /// Iterates `memblock_free_all`'s inputs, i.e.
    /// [`Memblock::free_mem_ranges`], so memory that is reserved or
    /// excluded by attributes (`NOMAP`, `DRIVER_MANAGED`) is not handed to
    /// the buddy allocator. Ranges are clipped to the managed arena by
    /// [`Buddy::free_range`].
    ///
    /// Only a shared borrow of the memblock is needed, never a mutable
    /// one: a memblock kept in a `static` behind a lock can be passed
    /// straight from its guard, because `&guard` coerces to `&Memblock`.
    /// The lock is held for the duration of the call; when that is not
    /// wanted, snapshot the ranges and use [`Buddy::free_ranges`] instead.
    ///
    /// Mirrors `memblock_free_all` and `free_low_memory_core_early`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFree`] if a range overlaps memory that is
    /// already free or in use.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Mutex;
    ///
    /// use the_buddy_system::Buddy;
    /// use the_buddy_system::Page;
    /// use the_memblock::flags::MemblockFlags;
    /// use the_memblock::memblock::Memblock;
    ///
    /// // As in a kernel: the boot allocator lives in a static behind a
    /// // lock, so there is no `&mut Memblock` to take.
    /// static MEMBLOCK: Mutex<Memblock<usize, 8>> = Mutex::new(Memblock::new());
    ///
    /// MEMBLOCK
    ///     .lock()
    ///     .unwrap()
    ///     .add(0x1_0000, 0x1_0000, MemblockFlags::NONE)
    ///     .unwrap();
    ///
    /// let mut pages = [Page::EMPTY; 64];
    /// let mut buddy = Buddy::<usize, 4>::new(0x1_0000, 0x1000, &mut pages).unwrap();
    ///
    /// let memblock = MEMBLOCK.lock().unwrap();
    /// buddy.free_memblock(&memblock).unwrap();
    /// assert_eq!(buddy.nr_free(), 16);
    /// ```
    ///
    /// [`Memblock::free_mem_ranges`]: the_memblock::memblock::Memblock::free_mem_ranges
    pub fn free_memblock<const N: usize>(&mut self, mb: &Memblock<A, N>) -> Result<(), Error> {
        self.free_ranges(mb.free_mem_ranges(MemblockFlags::NONE))
    }
}
