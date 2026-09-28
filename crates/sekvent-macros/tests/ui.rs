//! Compile-pass and compile-fail cases for `#[component]` and
//! `#[derive(ComponentError)]`. Pass cases also run.

#[test]
fn component_macros() {
    let cases = trybuild::TestCases::new();
    cases.pass("tests/ui/pass/*.rs");
    cases.compile_fail("tests/ui/fail/*.rs");
}
