use the_buddy_system::Buddy;
use the_buddy_system::Page;
use the_buddy_system::PageFrame;

#[derive(PageFrame)]
#[repr(transparent)]
struct Addr(u64);

fn main() {
    let mut pages = [Page::EMPTY; 64];
    let mut buddy = Buddy::<Addr, 4>::new(Addr(0), Addr(0x1000), &mut pages).unwrap();
    buddy.free_range(Addr(0), Addr(64 * 0x1000)).unwrap();
    let addr = buddy.alloc_pages(2).unwrap();
    assert_eq!(addr.0 % (4 * 0x1000), 0);
    buddy.free_pages(addr, 2).unwrap();
    assert_eq!(format!("{:?}", Addr(5)), "Addr(5)");
}
