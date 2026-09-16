//! Integration tests exercising the public API together with
//! `the-memblock`.

use core::ptr::NonNull;
use std::sync::Mutex;

use the_buddy_system::Buddy;
use the_buddy_system::Error;
use the_buddy_system::Page;
use the_buddy_system::PageFrame;
use the_memblock::PhysAddr;
use the_memblock::flags::MemblockFlags;
use the_memblock::memblock::Memblock;

const PS: usize = 0x1000;
const BASE: usize = 0x1_0000;
const PAGES: usize = 64;
const MAX_ORDER: usize = 4;

#[test]
fn boot_handoff_from_memblock_to_buddy() {
    let mut mb = Memblock::<usize, 16>::new();
    mb.add(BASE, PAGES * PS, MemblockFlags::NONE).unwrap();
    // The kernel image and a NOMAP page must not reach the buddy allocator.
    mb.reserve_kern(BASE, 2 * PS).unwrap();
    mb.mark_nomap(BASE + 0x2_0000, PS).unwrap();

    let mut pages = vec![Page::EMPTY; PAGES];
    let mut buddy = Buddy::<usize, MAX_ORDER>::new(BASE, PS, &mut pages).unwrap();
    buddy.free_memblock(&mb).unwrap();

    assert_eq!(buddy.nr_free(), PAGES - 3);
    buddy.validate().unwrap();

    // Allocate everything; reserved and NOMAP memory must never appear.
    let mut addrs = Vec::new();
    while let Ok(addr) = buddy.alloc_pages(0) {
        assert_eq!(addr % PS, 0);
        addrs.push(addr);
    }
    addrs.sort_unstable();

    let expected: Vec<usize> = (0..PAGES)
        .map(|idx| BASE + idx * PS)
        .filter(|addr| *addr != BASE && *addr != BASE + PS && *addr != BASE + 0x2_0000)
        .collect();
    assert_eq!(addrs, expected);
    assert!(matches!(buddy.alloc_pages(0), Err(Error::OutOfMemory)));
}

#[test]
fn feeding_the_same_free_ranges_twice_is_rejected() {
    let mut mb = Memblock::<usize, 16>::new();
    mb.add(BASE, PAGES * PS, MemblockFlags::NONE).unwrap();

    let mut pages = vec![Page::EMPTY; PAGES];
    let mut buddy = Buddy::<usize, MAX_ORDER>::new(BASE, PS, &mut pages).unwrap();
    buddy.free_memblock(&mb).unwrap();
    assert!(matches!(buddy.free_memblock(&mb), Err(Error::InvalidFree)));
}

#[test]
fn wide_physical_addresses_above_usize_range() {
    // On 32-bit x86 with PAE, `phys_addr_t` is 64-bit while `usize` is
    // 32-bit. The arena base can sit above 4 GiB; only the page offset
    // relative to the base is narrowed to `usize`.
    let base = 0x1_0000_0000u64;
    let mut pages = vec![Page::EMPTY; PAGES];
    let mut buddy = Buddy::<u64, MAX_ORDER>::new(base, PS as u64, &mut pages).unwrap();
    buddy
        .free_range(base, base + (PAGES * PS) as u64)
        .unwrap();
    assert_eq!(buddy.nr_free(), PAGES);

    let addr = buddy.alloc_pages(2).unwrap();
    assert!(addr >= base);
    assert_eq!((addr - base) % (4 * PS as u64), 0);
    buddy.free_pages(addr, 2).unwrap();
    assert_eq!(buddy.nr_free(), PAGES);
    buddy.validate().unwrap();
}

/// A static allocator, as a kernel would define it: the placeholder is the
/// constant initializer and the descriptors are supplied at boot.
static BUDDY: Mutex<Buddy<'static, usize, MAX_ORDER>> = Mutex::new(Buddy::uninit());

#[test]
fn static_placeholder_can_be_initialized() {
    let pages: &'static mut [Page] = Box::leak(vec![Page::EMPTY; PAGES].into_boxed_slice());
    let ptr = NonNull::new(pages.as_mut_ptr()).unwrap();

    let mut guard = BUDDY.lock().unwrap();
    assert!(!guard.is_initialized());
    // SAFETY: the leaked descriptor array lives for the rest of the
    // process, and it is only reached through this mutex.
    unsafe { guard.init(ptr, PAGES, BASE, PS) }.unwrap();
    guard.free_range(BASE, BASE + PAGES * PS).unwrap();
    assert_eq!(guard.nr_free(), PAGES);
    guard.validate().unwrap();
}

#[test]
fn kernel_style_raw_descriptor_array() {
    let mut pages = vec![Page::EMPTY; PAGES];
    let ptr = NonNull::new(pages.as_mut_ptr()).unwrap();

    // SAFETY: the descriptor array outlives the allocator (both are locals,
    // and `buddy` is dropped first) and all access goes through `buddy`.
    let mut buddy: Buddy<'static, usize, MAX_ORDER> =
        unsafe { Buddy::from_raw_parts(ptr, PAGES, BASE, PS) }.unwrap();

    buddy.free_range(BASE, BASE + PAGES * PS).unwrap();
    assert_eq!(buddy.nr_free(), PAGES);

    let addr = buddy.alloc_pages(0).unwrap();
    assert!((BASE..BASE + PAGES * PS).contains(&addr));
    assert_eq!((addr - BASE) % PS, 0);
    buddy.free_pages(addr, 0).unwrap();
    buddy.validate().unwrap();
}

#[test]
fn allocator_can_be_placed_in_a_lock() {
    fn assert_send<T: Send>() {}
    assert_send::<Buddy<'static, usize, MAX_ORDER>>();
}

/// A custom physical address newtype: `#[derive(PhysAddr)]` provides the
/// memblock trait, and [`PageFrame`] is implemented by forwarding to the
/// inner integer.
#[derive(PhysAddr)]
#[repr(transparent)]
struct Addr(u64);

impl PageFrame for Addr {
    fn try_to_usize(self) -> Option<usize> {
        usize::try_from(self.0).ok()
    }

    fn from_usize(index: usize) -> Self {
        Addr(index as u64)
    }
}

#[test]
fn custom_address_type_works_end_to_end() {
    let base = Addr(0x20_0000);
    let mut pages = vec![Page::EMPTY; PAGES];
    let mut buddy = Buddy::<Addr, MAX_ORDER>::new(base, Addr(PS as u64), &mut pages).unwrap();
    buddy
        .free_range(base, Addr(base.0 + (PAGES * PS) as u64))
        .unwrap();
    assert_eq!(buddy.nr_free(), PAGES);

    let addr = buddy.alloc_pages(3).unwrap();
    assert_eq!(addr.0 % (8 * PS as u64), 0);
    buddy.free_pages(addr, 3).unwrap();
    assert_eq!(buddy.nr_free(), PAGES);
    buddy.validate().unwrap();
}
