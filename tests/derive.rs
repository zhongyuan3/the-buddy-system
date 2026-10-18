//! End-to-end tests for the `#[derive(PageFrame)]` macro: a custom wrapper
//! type drives the full buddy API just like a primitive address.

use core::ptr::NonNull;

use the_buddy_system::Buddy;
use the_buddy_system::Page;
use the_buddy_system::PageFrame;
use the_buddy_system::PhysAddr;

const PS: u64 = 0x1000;

#[derive(PageFrame)]
#[repr(transparent)]
struct Addr(u64);

#[derive(PageFrame)]
struct UsizeAddr(usize);

#[test]
fn derived_constants() {
    assert_eq!(Addr::MAX, Addr(u64::MAX));
    assert_eq!(Addr::ZERO, Addr(0));
    assert_eq!(UsizeAddr::MAX, UsizeAddr(usize::MAX));
    assert_eq!(UsizeAddr::ZERO, UsizeAddr(0));
}

#[test]
fn derived_phys_addr_methods_delegate() {
    assert_eq!(Addr::align_up(Addr(1), Addr(PS)), Addr(PS));
    assert_eq!(Addr::align_down(Addr(0x1fff), Addr(PS)), Addr(PS));
    assert_eq!(Addr::pfn_up(Addr(1), Addr(PS)), Addr(1));
    assert_eq!(Addr::pfn_down(Addr(0xfff), Addr(PS)), Addr(0));
    assert_eq!(Addr::pfn_to_phys(Addr(2), Addr(PS)), Addr(0x2000));
    // Saturation near the top of the address space is preserved.
    assert_eq!(
        Addr::align_up(Addr(u64::MAX - 0x7f), Addr(0x100)),
        Addr(u64::MAX)
    );
}

#[test]
fn derived_page_frame_methods_delegate() {
    assert_eq!(Addr(0x1234).try_to_usize(), Some(0x1234));
    assert_eq!(Addr::from_usize(0x1234), Addr(0x1234));
    assert_eq!(UsizeAddr(0x1234).try_to_usize(), Some(0x1234));
    assert_eq!(UsizeAddr::from_usize(0x1234), UsizeAddr(0x1234));
}

#[test]
fn derived_supertrait_impls() {
    // Copy + Clone.
    let a = Addr(5);
    let b = a;
    assert_eq!(a, b);

    // PartialEq + Eq + PartialOrd + Ord.
    assert_eq!(Addr(1), Addr(1));
    assert_ne!(Addr(1), Addr(2));
    assert!(Addr(1) < Addr(2));
    assert_eq!(core::cmp::min(Addr(3), Addr(1)), Addr(1));

    // Add + Sub.
    assert_eq!(Addr(1) + Addr(2), Addr(3));
    assert_eq!(Addr(3) - Addr(1), Addr(2));

    // Debug.
    assert_eq!(format!("{:?}", Addr(5)), "Addr(5)");
}

#[test]
fn derived_type_drives_the_buddy_allocator() {
    let mut pages = [Page::EMPTY; 64];
    let mut buddy = Buddy::<Addr, 4>::new(Addr(0), Addr(PS), &mut pages).unwrap();
    buddy.free_range(Addr(0), Addr(64 * PS)).unwrap();
    assert_eq!(buddy.nr_free(), 64);

    let addr = buddy.alloc_pages(2).unwrap();
    assert_eq!(addr.0 % (4 * PS), 0);
    buddy.free_pages(addr, 2).unwrap();
    assert_eq!(buddy.nr_free(), 64);
    buddy.validate().unwrap();
}

#[test]
fn derived_type_works_with_raw_parts() {
    let mut pages = [Page::EMPTY; 64];
    let ptr = NonNull::new(pages.as_mut_ptr()).unwrap();

    // SAFETY: the descriptor array outlives the allocator (both are locals,
    // and `buddy` is dropped first) and all access goes through `buddy`.
    let mut buddy: Buddy<'static, Addr, 4> =
        unsafe { Buddy::from_raw_parts(ptr, 64, Addr(0), Addr(PS)) }.unwrap();

    buddy.free_range(Addr(0), Addr(64 * PS)).unwrap();
    let addr = buddy.alloc_pages(0).unwrap();
    assert_eq!(addr.0 % PS, 0);
    buddy.free_pages(addr, 0).unwrap();
    buddy.validate().unwrap();
}

#[test]
fn derived_type_works_with_memblock() {
    use the_memblock::flags::MemblockFlags;
    use the_memblock::memblock::Memblock;

    let mut mb = Memblock::<Addr, 8>::new();
    mb.add(Addr(0x1000), Addr(0x1000), MemblockFlags::NONE)
        .unwrap();
    let p = mb
        .phys_alloc(Addr(0x100), Addr(0x100), MemblockFlags::NONE)
        .unwrap();
    assert_eq!(p, Addr(0x1f00));
    assert!(mb.is_reserved(p));
    assert_eq!(mb.phys_mem_size(), Addr(0x1000));
}
