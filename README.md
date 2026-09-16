# the-buddy-system

A `no_std` reimplementation of the Linux kernel's [buddy page allocator]
(`mm/page_alloc.c`).

The allocator manages a contiguous range of physical page frames. Every
order has a free list of naturally aligned blocks; allocations split the
smallest block that fits (`expand`) and freed blocks coalesce with their
free buddies (`__free_one_page`), exactly like the kernel's per-zone
`free_area` arrays.

Unlike the kernel, the page descriptor array (the `struct page`
counterpart) is supplied by the caller as a `&mut [Page]` slice, so the
crate needs neither a global allocator nor `unsafe` while still mapping
onto a `vmemmap` style array placed in early boot memory.

[buddy page allocator]: https://www.kernel.org/doc/html/latest/core-api/memory-allocation.html

## Features

- Fixed-capacity, `no_std`, no allocation: the caller owns the page
  descriptor array. The allocator's own `unsafe` is confined to rebuilding
  the descriptor slice from the stored address and length.
- `MAX_ORDER` as a const generic (the kernel's `MAX_ORDER`), with runtime
  page sizes (4 KiB, 16 KiB, 64 KiB, ...).
- Address-width safe: absolute physical addresses stay in the address
  type, only bounded page offsets are narrowed to `usize`, so 32-bit
  targets with PAE-sized (64-bit `phys_addr_t`) physical addresses are
  supported.
- Kernel-faithful algorithms and bookkeeping: `expand`,
  `__free_one_page`, `__rmqueue_smallest`, `free_low_memory_core_early`,
  `free_area.nr_free` block counts and `zone->free_pages` page counts.
- Double free and invalid free detection through the per-block reference
  count, plus range overlap detection during boot handoff.
- `validate()` invariant checker (free list integrity, block alignment,
  accounting, coalescing canonical form) usable in tests and hardened
  builds.

## Usage

```rust
use the_buddy_system::Buddy;
use the_buddy_system::Page;

let mut pages = [Page::EMPTY; 64];
let mut buddy = Buddy::<u64, 4>::new(0, 0x1000, &mut pages).unwrap();

// Hand the allocator the memory that is actually free.
buddy.free_range(0, 64 * 0x1000).unwrap();
assert_eq!(buddy.nr_free(), 64);

let addr = buddy.alloc_pages(2).unwrap(); // 4 pages
assert_eq!(addr % (4 * 0x1000), 0);
buddy.free_pages(addr, 2).unwrap();
```

## Descriptor array storage

The allocator stores the descriptor array as an address plus a length and
rebuilds the slice inside each method call, so it can live in a `static`
behind a lock even though the kernel's `vmemmap` has no nameable borrow.
`Buddy::new` borrows a `&mut [Page]` (safe; convenient for tests and
embedded use), while `unsafe Buddy::from_raw_parts` takes a
`NonNull<Page>` and returns a `'static` allocator:

```rust
use core::ptr::NonNull;

let mut pages = vec![Page::EMPTY; 64];
let ptr = NonNull::new(pages.as_mut_ptr()).unwrap();

// SAFETY: the array outlives the allocator, and all access to it is
// serialized through `buddy` (for example by a spin lock in real use).
let mut buddy: Buddy<'static, usize, 4> =
    unsafe { Buddy::from_raw_parts(ptr, 64, 0x1_0000, 0x1000) }.unwrap();
buddy.free_range(0x1_0000, 0x5_0000).unwrap();
```

`Buddy` is `Send` when the address type is, so it can be moved into a
`SpinLock`; the lock provides the external synchronization the raw
constructor requires.

A `static` needs a constant initializer, but the descriptor address and
page count only become known at boot. `Buddy::uninit` is a `const`
placeholder for that, and the one-shot `unsafe Buddy::init` fills it in.
Until then, operations that need descriptors report
`Error::Uninitialized` instead of touching the dangling internal pointer:

```rust
use std::sync::Mutex;

static BUDDY: Mutex<Buddy<'static, usize, 11>> = Mutex::new(Buddy::uninit());

// After memory discovery, with the vmemmap mapped:
let ptr = core::ptr::NonNull::new(vmemmap).unwrap();
unsafe { BUDDY.lock().unwrap().init(ptr, nr_pages, base, page_size) }.unwrap();
```

## Boot handoff from memblock

The kernel leaves early boot by freeing what memblock did not reserve
(`memblock_free_all`). `the-buddy-system` mirrors that with
[`Buddy::free_memblock`], which walks the free memory ranges of a
[`the-memblock`] instance:

```rust
use the_buddy_system::Buddy;
use the_buddy_system::Page;
use the_memblock::flags::MemblockFlags;
use the_memblock::memblock::Memblock;

let mut mb = Memblock::<usize, 16>::new();
mb.add(0x1_0000, 0x4_0000, MemblockFlags::NONE).unwrap();
mb.reserve_kern(0x1_0000, 0x2000).unwrap(); // kernel image

let mut pages = vec![Page::EMPTY; 64];
let mut buddy = Buddy::<usize, 4>::new(0x1_0000, 0x1000, &mut pages).unwrap();
buddy.free_memblock(&mb).unwrap();

assert_eq!(buddy.nr_free(), 62);
```

[`Buddy::free_memblock`]: https://docs.rs/the-buddy-system/latest/the_buddy_system/buddy/struct.Buddy.html
[`the-memblock`]: https://crates.io/crates/the-memblock

## Address model

The allocator is generic over the physical address type, which implements
[`PageFrame`] (an extension of memblock's `PhysAddr`). As in the kernel,
where `phys_addr_t` can be wider than a pointer (36-bit physical
addresses on 32-bit x86 with PAE), absolute addresses are never narrowed
to `usize`: only page offsets relative to the arena base are, through a
checked conversion. All free list links and buddy arithmetic
(`idx ^ (1 << order)`) live in `usize` index space, which is isomorphic to
PFN space because the arena base is aligned to the largest block.

[`PageFrame`]: https://docs.rs/the-buddy-system/latest/the_buddy_system/pfn/trait.PageFrame.html

## Testing

`cargo test` runs the unit, integration and documentation tests. The
allocator rebuilds descriptor slices from stored addresses, so the tests
are also validated under Miri with both aliasing models:

```sh
cargo +nightly miri test --lib --tests
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-strict-provenance" cargo +nightly miri test --lib --tests
```

`random_operations_on_kernel_sized_arena` is ignored under Miri because
interpretation is too slow at that size; the same code paths run in
`random_operations_keep_invariants`.

## Roadmap

The core allocator is complete. The kernel layers built on top of it are
not implemented yet; the extension points are marked with `TODO(name)`
comments in the source:

- migration types (`__rmqueue_fallback`) — `TODO(migratetype)`
- zones, `GFP_*` flags, watermarks and per-CPU pagesets — `TODO(zone)`
- `buddy_merge_likely` bitmap and anti-fragmentation tail insertion —
  `TODO(bitmap)`, `TODO(antifrag)`
- guard pages and page poisoning — `TODO(guard)`
- pageblock helpers (isolation, hotplug) — `TODO(pageblock)`
- remaining `struct page` fields — `TODO(page)`

## License

MIT. See [LICENSE-MIT](LICENSE-MIT).
