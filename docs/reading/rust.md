<!-- Author: Nicola Antonello -->

# Modelling the domain in Rust

Types that carry their invariants, and making illegal states unrepresentable. Part of the [reading list](README.md).

- ★ Alexis King, [Parse, don't validate](https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/) (2019). Parse input once into types that carry their invariants, such as a transaction that cannot be unbalanced.
- Ralf Jung et al., [Safe Systems Programming in Rust](https://doi.org/10.1145/3418295) (Communications of the ACM, 2021). Ownership as an affine type system, so a moved value cannot be used again, and safe APIs whose private representation keeps its invariants whatever safe code does: the two mechanisms behind an ID that is not `Clone` and a token only the transition function can build.
- Cliff L. Biffle, [The Typestate Pattern in Rust](https://cliffle.com/blog/rust-typestate/) (2019). Encoding an object's state in its type, so that calls in the wrong state do not compile.
- Matt Noonan, [Ghosts of Departed Proofs (Functional Pearl)](https://doi.org/10.1145/3242744.3242755) (Haskell Symposium, 2018). Proofs as values that only the library can create, so a function can demand evidence that a check already ran: the academic form of a private-field token, cited by "Parse, don't validate".
- Falcon Darkstar Momot et al., [The Seven Turrets of Babel: A Taxonomy of LangSec Errors and How to Expunge Them](https://doi.org/10.1109/SecDev.2016.019) (IEEE SecDev, 2016). Names "shotgun parsing", validation scattered through processing code, and argues for rejecting invalid input at one boundary before any processing, which is why the API parses amounts into a positive type at the edge.
- Yaron Minsky, [Effective ML Revisited](https://blog.janestreet.com/effective-ml-revisited/) (Jane Street, 2011). Illustrates "make illegal states unrepresentable" with a before-and-after example.
- Philip Wadler, [Linear types can change the world!](https://homepages.inf.ed.ac.uk/wadler/topics/linear-logic.html#linear-types) (Programming Concepts and Methods, North-Holland, 1990). The origin of linear types in programming: values that can be neither duplicated nor discarded. Rust's affine types keep the first rule, which is why a `TransactionId` that is neither `Copy` nor `Clone` cannot be duplicated by accident.
