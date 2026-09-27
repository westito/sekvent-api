//! Compile-fail cases for `#[derive(EnvConfig)]`.

#[test]
fn derive_rejects_invalid_input() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/*.rs");
}
