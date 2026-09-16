# the-buddy-system

A `no_std` reimplementation of the Linux kernel's [buddy page allocator]
(`mm/page_alloc.c`).

The allocator manages a contiguous range of physical page frames. Every
order has a free list of naturally aligned blocks; allocations split the
smallest block that fits (`expand`) and freed blocks coalesce with their
free buddies (`__free_one_page`), exactly like the kernel's per-zone
`free_area` arrays. The page descriptor array (the `struct page`
counterpart) is supplied by the caller, so the crate needs no global
allocator.

This repository is a [Cargo workspace] with two crates:

| Crate | Description |
| --- | --- |
| [`the-buddy-system`](the-buddy-system/) | The allocator library, published to crates.io |
| [`the-buddy-system-derive`](the-buddy-system-derive/) | The `#[derive(PageFrame)]` proc-macro, re-exported by the main crate |

[buddy page allocator]: https://www.kernel.org/doc/html/latest/core-api/memory-allocation.html
[Cargo workspace]: https://doc.rust-lang.org/cargo/reference/workspaces.html

## Quick start

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

See the [`the-buddy-system`](the-buddy-system/) README for the full API,
features, the `#[derive(PageFrame)]` custom address type example, and the
memblock boot handoff, and
[`CHANGELOG.md`](the-buddy-system/CHANGELOG.md) for the release history.

## License

MIT. See [LICENSE](LICENSE).
