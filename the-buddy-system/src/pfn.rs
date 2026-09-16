//! The address type constraint used by the buddy allocator.
//!
//! The kernel keeps the buddy allocator's arithmetic in *page frame number*
//! (PFN) space (`include/linux/pfn.h`), never in physical address space,
//! because a physical address can be wider than a pointer: on 32-bit x86
//! with PAE, `phys_addr_t` is 64-bit (`CONFIG_PHYS_ADDR_T_64BIT`) while
//! `unsigned long` is 32-bit. Buddy math such as `pfn ^ (1 << order)` is
//! therefore performed on the pointer-width PFN.
//!
//! This crate follows the same split: absolute physical addresses stay in
//! the [`PhysAddr`] type `A`, while `usize` is used only for array indices,
//! free list links and bitmap positions. The two are bridged by
//! [`PageFrame`], and the only narrowing operation in the crate is the
//! checked [`PageFrame::try_to_usize`], which is applied exclusively to
//! *bounded* quantities: page offsets relative to the arena base and the
//! page size. Because the arena base is aligned to the largest block and
//! the managed page count fits in `usize`, an offset that passed
//! `index_of_pfn`'s range check is mathematically representable and the
//! conversion cannot truncate.

use the_memblock::PhysAddr;

/// A physical address type that can be split into PFN-space indices.
///
/// Implemented for all unsigned primitives; custom address newtypes (such
/// as the ones created with `#[derive(PhysAddr)]`) can implement it in
/// three lines by forwarding to the inner type.
///
/// # Contract
///
/// - [`try_to_usize`](PageFrame::try_to_usize) must return `None` if `self`
///   is not representable as a `usize`. It is only ever called with page
///   offsets and page sizes, never with absolute addresses.
/// - [`from_usize`](PageFrame::from_usize) must widen `index` exactly when
///   the type is at least as wide as `usize`. Narrower types may saturate;
///   [`Buddy::new`](crate::buddy::Buddy::new) rejects arenas whose page
///   count does not survive a `from_usize`/`try_to_usize` round trip.
pub trait PageFrame: PhysAddr {
    /// Narrows a bounded value (a page offset or a page size) to `usize`,
    /// returning `None` when the value is not representable.
    fn try_to_usize(self) -> Option<usize>;

    /// Widens a page index into this address type.
    ///
    /// Types narrower than `usize` saturate to their maximum instead of
    /// wrapping; the round trip check in `Buddy::new` guarantees that all
    /// indices used by an accepted arena are exact.
    fn from_usize(index: usize) -> Self;
}

macro_rules! impl_page_frame {
    ($($t:ty),+ $(,)?) => {
        $(
            impl PageFrame for $t {
                fn try_to_usize(self) -> Option<usize> {
                    usize::try_from(self).ok()
                }

                fn from_usize(index: usize) -> Self {
                    <$t>::try_from(index).unwrap_or(<$t>::MAX)
                }
            }
        )+
    };
}

impl_page_frame!(u8, u16, u32, u64, u128, usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_to_usize_accepts_representable_values() {
        assert_eq!(0u8.try_to_usize(), Some(0));
        assert_eq!(0xffu8.try_to_usize(), Some(0xff));
        assert_eq!(0x1234u32.try_to_usize(), Some(0x1234));
        assert_eq!(0x1234_5678u64.try_to_usize(), Some(0x1234_5678));
        assert_eq!(0x1234_5678usize.try_to_usize(), Some(0x1234_5678));
    }

    #[test]
    fn try_to_usize_rejects_wider_than_usize() {
        let too_wide = u128::from(u64::MAX) + 1;
        assert_eq!(too_wide.try_to_usize(), None);

        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(0x1_0000_0000u64.try_to_usize(), None);
        }
    }

    #[test]
    fn from_usize_widens_exactly() {
        assert_eq!(u64::from_usize(0x1234), 0x1234);
        assert_eq!(u128::from_usize(0x1234), 0x1234);
        assert_eq!(usize::from_usize(0x1234), 0x1234);
    }

    #[test]
    fn from_usize_saturates_for_narrow_types() {
        assert_eq!(u8::from_usize(0xff), 0xff);
        assert_eq!(u8::from_usize(0x100), u8::MAX);
        assert_eq!(u16::from_usize(0x1_0000), u16::MAX);
    }

    #[test]
    fn round_trip_is_identity_for_representable_indices() {
        for index in [0usize, 1, 0xff, 0x1000, 0x1234] {
            assert_eq!(u64::from_usize(index).try_to_usize(), Some(index));
            assert_eq!(usize::from_usize(index).try_to_usize(), Some(index));
        }
    }
}
