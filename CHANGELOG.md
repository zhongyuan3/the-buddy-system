# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Changed

- **Removed non-essential `Debug` implementations.** `Buddy`, `Page`,
  `PageFlags`, `FreeArea`, `FreeList`, `Link` and `FreeListIter` no longer
  implement `Debug`; only `Error` keeps it. Internal assertions and tests
  compare values directly instead of formatting them. This is a breaking
  change for code that relied on the derived formatting. The `PageFrame`
  derive macro no longer generates `Debug` either; add it to the derive list
  explicitly if needed.
- **Flattened the module layout.** Public items are exported only from the
  crate root. The `error`, `order` and `pfn` modules were removed: `Error`
  lives in `buddy.rs`, the order helpers live in `page.rs`, and
  `PageFrame` is re-exported directly from `the-mm-core`. `FreeArea` is now
  internal to `buddy.rs`.
- **No memblock dependency at all.** The `memblock` feature,
  `Buddy::free_memblock` and the `memblock` module were removed. The boot
  handoff is explicit:
  `buddy.free_ranges(mb.free_mem_ranges(MemblockFlags::NONE))`. Any boot
  allocator, or a snapshot of ranges, can drive `free_ranges`.
- **The address traits moved to `the-mm-core`.** `PageFrame` (and its
  `PhysAddr` supertrait) now live in the shared `the-mm-core` crate.
  `the_buddy_system::PageFrame` and `the_buddy_system::PhysAddr` are
  re-exports, so existing code and `#[derive(PageFrame)]` keep working
  unchanged.
- **PFN conversions moved to `PageFrame`.** `PhysAddr` is now purely
  address/alignment arithmetic. `PageFrame` provides `phys_to_pfn` (floor),
  `pfn_to_phys` and `pfn_ceil` (exact ceil); `pfn_down` was removed and
  `pfn_up` renamed. `#[derive(PageFrame)]` now generates the PFN methods in
  the `PageFrame` implementation.
- The buddy crate depends only on `the-mm-core`; `the-memblock` is used
  only as a dev-dependency by the integration tests.
- `Buddy::alloc_pages` now stores the allocated order in the block head,
  and `Buddy::free_pages` rejects a free whose order does not match. This
  turns an order mismatch into `Error::InvalidFree` instead of corrupting
  the free lists.
- Arena validation uses the new `PageFrame::try_from_usize` instead of a
  manual `from_usize`/`try_to_usize` round trip.
- `Buddy::nr_free_blocks` remains the public introspection API for
  per-order free counts; `FreeArea` is internal to `buddy.rs`.
- **Arbitrary page-aligned arenas.** The base no longer has to be aligned to
  the largest block and the arena may be smaller than `1 << MAX_ORDER`
  pages. Buddy alignment and coalescing are computed in absolute PFN space;
  only the arena base needs page alignment. `Buddy::free_pages` also checks
  block alignment in PFN space rather than index space.
- The `the-memblock` dev-dependency is now `0.3.0`. Integration tests use
  its crate-root exports and the flag-less `Memblock::add`; the custom
  `RegionFlags` handoff test was removed along with that API.

### Compatibility

- Replace `buddy.free_memblock(&mb)` with
  `buddy.free_ranges(mb.free_mem_ranges(MemblockFlags::NONE))` and remove
  `features = ["memblock"]`.

## [0.1.0]

### Added

- Initial release: a `no_std` buddy page allocator mirroring the Linux
  kernel's `mm/page_alloc.c`.
- `#[derive(PageFrame)]`, provided by the new `the-buddy-system-derive`
  crate and re-exported at the crate root alongside the `PageFrame` trait.
  It implements `PageFrame`, its memblock `PhysAddr` supertrait and every
  required supertrait for single-field tuple structs wrapping another
  `PageFrame` implementor, e.g. `#[derive(PageFrame)] struct Addr(u64);`.
- `Buddy<'a, A, NR_PAGE_ORDERS>` over a caller-provided page descriptor
  array, with `free_range` (`free_low_memory_core_early`), `alloc_pages`
  (`__rmqueue_smallest` plus `expand`) and `free_pages` (`__free_pages`
  with `free_pages_prepare` state checks). The const generic is the number
  of free areas, mirroring the kernel's `NR_PAGE_ORDERS`: valid orders are
  `0..=MAX_ORDER`, where the associated constant `Buddy::MAX_ORDER` is
  `NR_PAGE_ORDERS - 1` (the kernel's `MAX_PAGE_ORDER`), and the largest
  block is `1 << MAX_ORDER` pages (`MAX_ORDER_NR_PAGES`).
- Descriptor array stored as an address plus a length; the slice is
  rebuilt inside each call. `Buddy::new` borrows a `&mut [Page]` safely,
  and `unsafe Buddy::from_raw_parts` returns a `'static` allocator for
  kernel `vmemmap` arrays that have no nameable borrow. `Buddy` implements
  `Send` so it can live in a caller-provided lock.
- `Buddy::uninit`, a `const` placeholder for static definitions, plus the
  one-shot `unsafe Buddy::init` that supplies the descriptors at boot.
  Operations before initialization report `Error::Uninitialized`, and a
  second `init` reports `Error::AlreadyInitialized`.
- `PageFrame` address trait: checked narrowing of bounded page offsets and
  widening of indices, so 32-bit targets with wider physical address types
  never truncate addresses.
- `free_memblock` boot handoff that frees the `memblock` free memory
  ranges into the allocator (`memblock_free_all`). It needs only a shared
  borrow, so a memblock kept in a `static` behind a lock can be passed
  straight from its guard.
- `free_ranges` for feeding a pre-collected list of ranges, for example a
  snapshot of the memblock free ranges taken before the memblock lock is
  released.
- `validate()` invariant checker covering free list integrity, block
  alignment, the page and block accounting, and the coalescing canonical
  form.
- `Error` variants for out-of-memory, invalid orders and addresses,
  invalid frees (including double frees and overlapping boot ranges) and
  geometry violations.
