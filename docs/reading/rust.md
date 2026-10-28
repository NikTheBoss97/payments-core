<!-- Author: Nicola Antonello -->

# Modelling the domain in Rust

Types that carry their invariants, and making illegal states unrepresentable. Part of the [reading list](README.md).

- ★ Alexis King, [Parse, don't validate](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/) (2019). Parse input once into types that carry their invariants, such as a transaction that cannot be unbalanced.
- Cliff L. Biffle, [The Typestate Pattern in Rust](https://cliffle.com/blog/rust-typestate/) (2019). Encoding an object's state in its type, so that calls in the wrong state do not compile.
- Yaron Minsky, [Effective ML Revisited](https://blog.janestreet.com/effective-ml-revisited/) (Jane Street, 2011). Illustrates "make illegal states unrepresentable" with a before-and-after example.
