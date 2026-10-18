//! The address type constraint used by the buddy allocator.
//!
//! [`PageFrame`] lives in [`the_mm_core`] and is shared with the slab
//! allocator. This module re-exports it under the historical
//! `the_buddy_system::pfn` path.
//!
//! The kernel keeps buddy arithmetic in *page frame number* (PFN) space
//! (`include/linux/pfn.h`), never in physical address space, because a
//! physical address can be wider than a pointer. [`PageFrame`] bridges the
//! address type and the `usize` index space: only page offsets relative to
//! the arena base are narrowed, through a checked conversion.

pub use the_mm_core::pfn::PageFrame;
