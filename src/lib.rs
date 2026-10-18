//! A `no_std` reimplementation of the Linux kernel's buddy page allocator.
//!
//! The allocator manages a contiguous range of physical page frames using
//! the buddy algorithm from `mm/page_alloc.c`: every order has a free list
//! of naturally aligned blocks, allocations are served by splitting the
//! smallest block that fits (`expand`), and freed blocks coalesce with
//! their free buddies (`__free_one_page`).
//!
//! The page descriptor array (the `struct page` counterpart, [`Page`]) is
//! supplied by the caller, so the crate needs no global allocator. The
//! allocator stores it as an address plus a length and rebuilds the slice
//! inside each method call — the library's equivalent of entering the
//! caller's critical section. Two constructors cover the use cases:
//!
//! - [`Buddy::new`] borrows a `&'a mut [Page]`, which is convenient for
//!   tests and embedded use and keeps the borrow checked.
//! - `unsafe` [`Buddy::from_raw_parts`] takes a raw pointer and yields a
//!   `'static` allocator, matching a kernel `vmemmap` that lives at a fixed
//!   address with no nameable borrow. The caller promises the array
//!   outlives the allocator and that access to it is externally
//!   synchronized, typically by keeping the allocator in a spin lock.
//!
//! The typical boot sequence mirrors the kernel's:
//!
//! 1. Record physical memory with [`the_memblock`] and reserve the kernel
//!    image and other early allocations.
//! 2. Create the page descriptor array and hand it to [`Buddy::new`] (or
//!    [`Buddy::from_raw_parts`] once the `vmemmap` is mapped).
//! 3. Free the remaining memory with [`Buddy::free_memblock`]
//!    (`memblock_free_all`).
//! 4. Serve page allocations through [`Buddy::alloc_pages`] and
//!    [`Buddy::free_pages`].
//!
//! # Address model
//!
//! The allocator is generic over the physical address type `A`, which only
//! needs to implement [`PageFrame`] (an extension of memblock's
//! [`PhysAddr`]). As in the kernel, where `phys_addr_t` can be wider than
//! a pointer (36-bit physical addresses on 32-bit x86 with PAE), absolute
//! addresses are never narrowed to `usize`; only page offsets relative to
//! the arena base are, through a checked conversion.
//!
//! The [`PageFrame`] trait is implemented for every unsigned primitive;
//! the [`#[derive(PageFrame)]`](crate::PageFrame) macro transparently
//! implements it, together with the supertraits it requires, for
//! single-field wrapper types such as `struct Addr(u64)`.
//!
//! # Kernel references
//!
//! - `mm/page_alloc.c`: `__free_one_page`, `__rmqueue_smallest`,
//!   `expand`, `free_low_memory_core_early`, `prep_new_page`
//! - `include/linux/mmzone.h`: `struct free_area`, `struct zone`,
//!   `MAX_PAGE_ORDER`, `NR_PAGE_ORDERS`
//! - `include/linux/page-flags.h`: `PG_buddy`
//! - `include/linux/mm_types.h`: `struct page`
//!
//! # Roadmap
//!
//! The core allocator is complete. The kernel layers built on top of it
//! are intentionally left for later; the extension points are marked with
//! `TODO(name)` comments in the source:
//!
//! - `TODO(migratetype)`: migration types and `__rmqueue_fallback`.
//! - `TODO(zone)`: zones, `GFP_*` flags, watermarks and per-CPU pagesets.
//! - `TODO(bitmap)` / `TODO(antifrag)`: the `buddy_merge_likely` bitmap
//!   and the anti-fragmentation tail insertion.
//! - `TODO(guard)`: guard pages (`set_page_guard`) and page poisoning.
//! - `TODO(pageblock)`: pageblock helpers for isolation and hotplug.
//! - `TODO(page)`: the remaining `struct page` fields.
//!
//! [`PhysAddr`]: the_memblock::PhysAddr

#![no_std]

pub use crate::buddy::Buddy;
pub use crate::error::Error;
pub use crate::page::Page;
pub use crate::page::PageFlags;
pub use crate::pfn::PageFrame;
pub use the_buddy_system_derive::PageFrame;
pub use the_memblock::addr::PhysAddr;

pub mod buddy;
pub mod error;
pub mod free_area;
pub mod memblock;
pub mod order;
pub mod page;
pub mod pfn;

pub(crate) mod list;
