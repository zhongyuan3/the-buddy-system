//! Compile-time (trybuild) tests for the `#[derive(PageFrame)]` macro.

// trybuild invokes cargo and the filesystem, which Miri's isolation does
// not provide; the macro itself is compile-time code and generates safe,
// fully delegated implementations that the `derive` test target covers.
#[cfg_attr(miri, ignore)]
#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass/*.rs");
    t.compile_fail("tests/ui/fail/*.rs");
}
