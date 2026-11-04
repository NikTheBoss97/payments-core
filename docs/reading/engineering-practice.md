<!-- Author: Nicola Antonello -->

# Engineering practice

Continuous integration, the software supply chain, code scanning and recording decisions. Part of the [reading list](README.md).

## Continuous integration

- ★ GitHub, [Workflow syntax for GitHub Actions](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax) (GitHub Docs). Every key of a workflow file: events, jobs, steps, service containers, expressions and permissions.
- GitHub, [Secure use reference](https://docs.github.com/en/actions/reference/security/secure-use) (GitHub Docs). Why to pin each action to a full commit SHA rather than a tag, and to give the workflow's token only the permissions it needs.
- Igibek Koishybayev et al., [Characterizing the Security of Github CI Workflows](https://www.usenix.org/conference/usenixsecurity22/presentation/koishybayev) (USENIX Security, 2022). Across 447,238 workflows, 99.8% gave the token more access than read-only and about 1% of third-party action references were commit hashes; a pinned action can still run actions it references by tag, so pinning is necessary but not sufficient.
- Michael Hilton et al., [Usage, Costs, and Benefits of Continuous Integration in Open-Source Projects](https://doi.org/10.1145/2970276.2970358) (ASE, 2016). Data from 34,544 GitHub projects, 1,529,291 builds and 442 developers: 40% of the projects used CI, and projects with CI released about twice as often.

## The software supply chain and code scanning

- ★ Piergiorgio Ladisa et al., [SoK: Taxonomy of Attacks on Open-Source Software Supply Chains](https://doi.org/10.1109/SP46215.2023.10179304) (IEEE S&P, 2023). An attack tree of 107 vectors, tied to 94 real incidents and mapped to 33 safeguards: a way to see how much a given set of checks leaves open.
- ★ Rust Secure Code Working Group, [RustSec Advisory Database](https://rustsec.org/). Advisories filed against crates published on crates.io, and the data `cargo-deny` reads. A crate taken from a git commit does not match them, which is why this project checks its pinned sqlx crates a second way.
- Embark Studios, [cargo-deny](https://embarkstudios.github.io/cargo-deny/) (documentation). The checks for advisories, licences, bans and sources, and each option of `deny.toml`.
- GitHub, [About Dependabot alerts](https://docs.github.com/en/code-security/dependabot/dependabot-alerts/about-dependabot-alerts) (GitHub Docs). Only advisories that GitHub has reviewed trigger an alert, so an advisory can be public in RustSec with no alert to show for it.
- GitHub, [About code scanning with CodeQL](https://docs.github.com/en/code-security/code-scanning/introduction-to-code-scanning/about-code-scanning-with-codeql) (GitHub Docs). A database built from the code, queries run over it, and the difference between default and advanced setup.
- William Woodruff, [We should all be using dependency cooldowns](https://blog.yossarian.net/2025/11/21/We-should-all-be-using-dependency-cooldowns) (2025). In a small sample of recent attacks, 8 of 10 had a window under a week, so waiting a few days before adopting a release avoids most of them. The author calls it no panacea.
- Wachiraphan Charoenwet et al., [An Empirical Study of Static Analysis Tools for Secure Code Review](https://doi.org/10.1145/3650212.3680313) (ISSTA, 2024). On 815 commits that introduced vulnerabilities in C and C++ projects, one tool flagged the vulnerable function in 52% of them, at least 76% of the warnings there were irrelevant to the commit, and 22% of the commits went undetected. Static analysis is useful and proves nothing.
- Runzhi He et al., [Automating Dependency Updates in Practice: An Exploratory Study on GitHub Dependabot](https://doi.org/10.1109/TSE.2023.3278129) (IEEE Transactions on Software Engineering, 2023). Projects that adopt Dependabot reduce their technical lag, but developers often configure it to send fewer notifications, and 11.3% of the projects studied had dropped it for an alternative.
- Hao Li et al., [An Empirical Study of Yanked Releases in the Rust Package Registry](https://doi.org/10.1109/TSE.2022.3152148) (IEEE Transactions on Software Engineering, 2023). 9.6% of Cargo packages have a yanked release and 46% of packages directly adopted one. A lock file keeps using a yanked version, so noticing takes a check.
- Nasif Imtiaz et al., [A Comparative Study of Vulnerability Reporting by Software Composition Analysis Tools](https://doi.org/10.1145/3475716.3475769) (ESEM, 2021). Nine tools run on one application reported widely different results, largely because of their databases, and the authors advise against relying on a single tool.
- Pavel Avgustinov et al., [QL: Object-oriented Queries on Relational Data](https://doi.org/10.4230/LIPIcs.ECOOP.2016.2) (ECOOP, 2016). The origin of the query language underlying CodeQL: classes and methods reinterpreted in logic, compiled to Datalog, scaling to millions of lines of code.

## Recording decisions

- ★ Michael Nygard, [Documenting Architecture Decisions](https://www.cognitect.com/blog/2011/11/15/documenting-architecture-decisions) (Cognitect, 2011). The original ADR format: context, decision, status and consequences, with a reversed decision kept and marked superseded.
- Georg Buchgeher et al., [Using Architecture Decision Records in Open Source Projects—An MSR Study on GitHub](https://doi.org/10.1109/ACCESS.2023.3287654) (IEEE Access, 2023). ADR adoption on GitHub is low but rising, about half the repositories with ADRs hold only one to five, sustained use is a team activity over a long period, and most follow Nygard's template.
- David L. Parnas and Paul C. Clements, [A Rational Design Process: How and Why to Fake It](https://doi.org/10.1109/TSE.1986.6312940) (IEEE Transactions on Software Engineering, 1986). Design documents should read as if the design had been derived rationally, and record every alternative considered and why it was rejected: the origin of the options tables in this project's ADRs.
