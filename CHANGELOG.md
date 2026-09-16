# Changelog

All notable changes to this project are documented in this file.

## [0.1.0]

### Added

- Initial release: a `no_std` buddy page allocator mirroring the Linux
  kernel's `mm/page_alloc.c`.
- `Buddy<'a, A, MAX_ORDER>` over a caller-provided page descriptor array,
  with `free_range` (`free_low_memory_core_early`), `alloc_pages`
  (`__rmqueue_smallest` plus `expand`) and `free_pages` (`__free_pages`
  with `free_pages_prepare` state checks).
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
  ranges into the allocator (`memblock_free_all`).
- `validate()` invariant checker covering free list integrity, block
  alignment, the page and block accounting, and the coalescing canonical
  form.
- `Error` variants for out-of-memory, invalid orders and addresses,
  invalid frees (including double frees and overlapping boot ranges) and
  geometry violations.
