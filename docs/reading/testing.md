<!-- Author: Nicola Antonello -->

# Testing for correctness

Property-based testing, failure injection and testing database guarantees. Part of the [reading list](README.md).

- ★ Ding Yuan et al., [Simple Testing Can Prevent Most Critical Failures](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/yuan) (OSDI, 2014). Most catastrophic failures studied came from mishandled non-fatal errors, so failure injection should target error paths.
- Koen Claessen and John Hughes, [QuickCheck: A Lightweight Tool for Random Testing of Haskell Programs](https://doi.org/10.1145/351240.351266) (ICFP, 2000). The paper that introduced QuickCheck, from which modern property-based testing descends.
- John Hughes, [Experiences with QuickCheck: Testing the Hard Stuff and Staying Sane](https://publications.lib.chalmers.se/records/fulltext/232550/local_232550.pdf) (2016). Testing stateful systems against a model of their state transitions, and finding race conditions.
- Jason Lingle, Mazdak Farrokhzad and contributors, [Proptest](https://proptest-rs.github.io/proptest/) (the proptest book). Property-based testing in Rust: strategies, shrinking, failure persistence and state-machine tests.
- Jingyu Zhou et al., [FoundationDB: A Distributed Unbundled Transactional Key Value Store](https://doi.org/10.1145/3448016.3457559) (SIGMOD, 2021). Section 4 describes deterministic simulation, which makes every injected failure reproducible.
- Kyle Kingsbury, [Jepsen: PostgreSQL 12.3](https://jepsen.io/analyses/postgresql-12.3) (2020). Testing isolation guarantees instead of assuming them; the SERIALIZABLE bug it found was fixed in 12.4.
