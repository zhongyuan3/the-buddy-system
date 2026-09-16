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

impl<A: PageFrame, const MAX_ORDER: usize> Buddy<'_, A, MAX_ORDER> {
    /// Frees every free memory range tracked by `mb` into the allocator.
    ///
    /// Iterates `memblock_free_all`'s inputs, i.e.
    /// [`Memblock::free_mem_ranges`], so memory that is reserved or
    /// excluded by attributes (`NOMAP`, `DRIVER_MANAGED`) is not handed to
    /// the buddy allocator. Ranges are clipped to the managed arena by
    /// [`Buddy::free_range`].
    ///
    /// Mirrors `memblock_free_all` and `free_low_memory_core_early`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidFree`] if a range overlaps memory that is
    /// already free or in use.
    ///
    /// [`Memblock::free_mem_ranges`]: the_memblock::memblock::Memblock::free_mem_ranges
    pub fn free_memblock<const N: usize>(&mut self, mb: &Memblock<A, N>) -> Result<(), Error> {
        for (base, end) in mb.free_mem_ranges(MemblockFlags::NONE) {
            self.free_range(base, end)?;
        }
        Ok(())
    }
}
