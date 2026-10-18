//! Order and block-size arithmetic.
//!
//! TODO(pageblock): add `pageblock_order` / `pageblock_nr_pages` helpers
//! once pageblocks (anti-fragmentation, memory isolation and hotplug) are
//! implemented.

/// Returns the number of pages in a block of `order`, i.e. `1 << order`.
///
/// Mirrors the kernel's block sizes derived from `MAX_PAGE_ORDER` /
/// `NR_PAGE_ORDERS`, such as `MAX_ORDER_NR_PAGES`.
///
/// # Panics
///
/// Panics if `order >= usize::BITS`.
pub const fn pages_in_order(order: u8) -> usize {
    1usize << order
}

/// Returns the number of pages in the largest block an arena with the given
/// (inclusive) `MAX_ORDER` can allocate, i.e. `1 << MAX_ORDER`.
///
/// Mirrors the kernel's `MAX_ORDER_NR_PAGES`.
pub const fn max_block_pages(max_order: u8) -> usize {
    pages_in_order(max_order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_in_order_is_a_power_of_two() {
        assert_eq!(pages_in_order(0), 1);
        assert_eq!(pages_in_order(1), 2);
        assert_eq!(pages_in_order(3), 8);
        assert_eq!(pages_in_order(10), 1024);
        assert_eq!(pages_in_order(12), 4096);
    }

    #[test]
    fn max_block_pages_matches_buddy_orders() {
        // MAX_ORDER is inclusive, so 1 << MAX_ORDER is the largest block.
        assert_eq!(max_block_pages(0), 1);
        assert_eq!(max_block_pages(3), 8);
        assert_eq!(max_block_pages(10), 1024);
    }
}
