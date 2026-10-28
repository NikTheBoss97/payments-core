<!-- Author: Nicola Antonello -->

# Engineering practice

Continuous integration and recording decisions. Part of the [reading list](README.md).

## Continuous integration

- ★ GitHub, [Workflow syntax for GitHub Actions](https://docs.github.com/en/actions/reference/workflows-and-actions/workflow-syntax) (GitHub Docs). Every key of a workflow file: events, jobs, steps, service containers, expressions and permissions.
- GitHub, [Secure use reference](https://docs.github.com/en/actions/reference/security/secure-use) (GitHub Docs). Why to pin each action to a full commit SHA rather than a tag, and to give the workflow's token only the permissions it needs.
- Igibek Koishybayev et al., [Characterizing the Security of Github CI Workflows](https://www.usenix.org/conference/usenixsecurity22/presentation/koishybayev) (USENIX Security, 2022). Across 447,238 workflows, 99.8% gave the token more access than read-only and about 1% of third-party action references were commit hashes; a pinned action can still run actions it references by tag, so pinning is necessary but not sufficient.
- Michael Hilton et al., [Usage, Costs, and Benefits of Continuous Integration in Open-Source Projects](https://doi.org/10.1145/2970276.2970358) (ASE, 2016). Data from 34,544 GitHub projects, 1,529,291 builds and 442 developers: 40% of the projects used CI, and projects with CI released about twice as often.

## Recording decisions

- ★ Michael Nygard, [Documenting Architecture Decisions](https://www.cognitect.com/blog/2011/11/15/documenting-architecture-decisions) (Cognitect, 2011). The original ADR format: context, decision, status and consequences, with a reversed decision kept and marked superseded.
- Georg Buchgeher et al., [Using Architecture Decision Records in Open Source Projects—An MSR Study on GitHub](https://doi.org/10.1109/ACCESS.2023.3287654) (IEEE Access, 2023). ADR adoption on GitHub is low but rising, about half the repositories with ADRs hold only one to five, sustained use is a team activity over a long period, and most follow Nygard's template.
- David L. Parnas and Paul C. Clements, [A Rational Design Process: How and Why to Fake It](https://doi.org/10.1109/TSE.1986.6312940) (IEEE Transactions on Software Engineering, 1986). Design documents should read as if the design had been derived rationally, and record every alternative considered and why it was rejected: the origin of the options tables in this project's ADRs.
