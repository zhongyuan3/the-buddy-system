//! The error type returned by fallible buddy allocator operations.

/// Errors returned by the buddy allocator.
///
/// The variants mirror the failure modes the kernel's page allocator
/// asserts on (`bad_page`, `VM_BUG_ON_PAGE`, `check_new_page`) and the
/// `NULL`/`-ENOMEM` result of `__rmqueue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// No free block of the requested order (or any larger order) exists.
    OutOfMemory,
    /// The requested order is not below `MAX_ORDER`.
    InvalidOrder,
    /// The address is misaligned, does not denote the start of a block, or
    /// lies outside the managed range.
    InvalidAddress,
    /// The block may not be freed: it is already free, in use, or the
    /// freed range overlaps pages that are not free. Also reported when a
    /// free range overlaps an already freed region.
    InvalidFree,
    /// The page size is zero, not a power of two, or not representable.
    InvalidPageSize,
    /// The page descriptor array is unusable, for example because the
    /// pointer passed to [`Buddy::from_raw_parts`] is not aligned to
    /// [`Page`].
    ///
    /// [`Buddy::from_raw_parts`]: crate::buddy::Buddy::from_raw_parts
    /// [`Page`]: crate::page::Page
    InvalidPageArray,
    /// An operation that needs page descriptors was attempted on an
    /// allocator created by [`Buddy::uninit`] whose [`Buddy::init`] has not
    /// run yet.
    ///
    /// [`Buddy::uninit`]: crate::buddy::Buddy::uninit
    /// [`Buddy::init`]: crate::buddy::Buddy::init
    Uninitialized,
    /// [`Buddy::init`] was called on an allocator that already manages
    /// pages.
    ///
    /// [`Buddy::init`]: crate::buddy::Buddy::init
    AlreadyInitialized,
    /// The arena geometry is invalid: fewer pages than `1 << (MAX_ORDER - 1)`,
    /// a base address not aligned to the largest block, or an address range
    /// that would wrap around the top of the address space.
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

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::string::ToString;

    use super::*;

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
