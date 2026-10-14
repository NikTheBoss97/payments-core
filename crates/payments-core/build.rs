// Author: Nicola Antonello

// The migrations are embedded at compile time (`#[sqlx::test]` and
// `sqlx::migrate!`), but Cargo only rebuilds when Rust sources change.
// Without this, a new migration file would not reach the tests.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
