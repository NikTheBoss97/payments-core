// Author: Nicola Antonello

//! Integer overflow panics instead of wrapping, in every profile (ADR-0003).
//! Plain `cargo test` runs this file in the test profile, which inherits the
//! dev profile, where Cargo checks overflow by default. CI also runs it with
//! `--release`, where Cargo wraps by default unless the root Cargo.toml turns
//! the checks on.

#[test]
#[should_panic(expected = "attempt to add with overflow")]
fn integer_overflow_panics() {
    // The inner black_box hides the value from the compiler, which would
    // otherwise reject this overflow at compile time. The outer one consumes
    // the sum, which rustc would flag as an unused arithmetic operation.
    std::hint::black_box(std::hint::black_box(i64::MAX) + 1);
}
