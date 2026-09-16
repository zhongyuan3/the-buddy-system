# the-buddy-system-derive

Derive macro for the [`PageFrame`] trait from [the-buddy-system].

`#[derive(PageFrame)]` implements `PageFrame`, its memblock `PhysAddr`
supertrait and every supertrait they require (`Clone`, `Copy`,
`PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Debug`, `Add`, `Sub`) for a
single-field tuple struct wrapping another `PageFrame` implementor. All
address arithmetic and index conversions are delegated to the inner type,
so the overflow-safety and panicking guarantees of the traits are
preserved.

```rust
use the_buddy_system::Buddy;
use the_buddy_system::Page;
use the_buddy_system::PageFrame;

#[derive(PageFrame)]
#[repr(transparent)]
struct Addr(u64);

let mut pages = [Page::EMPTY; 64];
let mut buddy = Buddy::<Addr, 4>::new(Addr(0), Addr(0x1000), &mut pages).unwrap();
buddy.free_range(Addr(0), Addr(64 * 0x1000)).unwrap();

let base = buddy.alloc_pages(2).unwrap();
assert_eq!(base.0 % (4 * 0x1000), 0);
buddy.free_pages(base, 2).unwrap();
```

The macro is re-exported at the root of `the-buddy-system`, so depending
on that crate is enough; this crate is only needed when the derive is
used directly.

Only concrete, non-generic tuple structs with exactly one field are
supported, and the field must already implement `PageFrame` (all unsigned
primitives do). Because `PageFrame` requires `PhysAddr`, the derive
implements the memblock trait as well: do not also derive
`the_memblock::PhysAddr` for the same type, the implementations would
conflict.

[`PageFrame`]: https://docs.rs/the-buddy-system/latest/the_buddy_system/pfn/trait.PageFrame.html
[the-buddy-system]: https://crates.io/crates/the-buddy-system

## License

MIT. See [LICENSE](LICENSE).
