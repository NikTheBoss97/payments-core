<!-- Author: Nicola Antonello -->

# ADR-0027: Supply-chain checks

- Status: accepted
- Date: 2026-11-04

## Context

`Cargo.lock` names 164 crates this project did not write, and until now nothing
looked at them. CI builds and tests them, but a test of this project's behaviour
says nothing about a published vulnerability in a dependency, a licence the project
cannot accept, or a crate that starts coming from somewhere new.

Running a checker on `main` for the first time, on 2026-11-04, found a real
problem. `ctutils` 0.4.3 had a published advisory,
[RUSTSEC-2026-0385](https://rustsec.org/advisories/RUSTSEC-2026-0385), dated
2026-10-23: code meant to run in constant time could branch on a secret. This
program runs that code. At login, `sqlx-postgres` checks the server's SCRAM
signature with `verify_slice`, which `digest` implements with `ctutils`. Reading
the code, an attacker would have to pose as the database and time that comparison,
so the risk was small, but no check had noticed.

GitHub's Advisory Database could not have reported it. On the same day it held no
entry for the crate, reviewed or unreviewed, 12 days after RustSec's. The same
query returned 6 reviewed advisories for `tokio` and 2 for `cmov`, and the newest
reviewed Rust advisory was dated 2026-10-30, so the query and the database were
working. Dependabot raises alerts only for advisories GitHub has reviewed.

A check that reports nothing proves little until it has been seen to report
something, so each check below was run against planted problems, on a scratch copy
or on a branch that was never merged. That found two blind spots, which shaped the
decision: `cargo-deny` cannot match an advisory to the sqlx crates, and CodeQL does
not report SQL injection in the form sqlx 0.9 requires.

## Options

What to run:

| Option | Why not |
|---|---|
| **`cargo-deny`, Dependabot and CodeQL in GitHub Actions** | Chosen |
| Dependabot alone | It misses what GitHub has not reviewed, as above, and checks neither licences nor sources. |
| `cargo audit` in place of `cargo-deny` | It reads the same database and checks neither licences nor sources. |
| A GitLab server on a development machine, with its own pipeline | A reviewer of this repository cannot see it, it needs upkeep, and GitHub already runs this project's CI. |

How CI gets `cargo-deny`:

| Option | Why not |
|---|---|
| **`cargo install --locked --version`, cached by version** | Chosen. A crates.io version can never be overwritten, `--locked` builds the versions in the crate's own lock file, and Cargo verifies each checksum. CI installs `sqlx-cli` this way too. |
| The vendor's action, pinned to a commit | Dependabot would keep it current. But its Dockerfile pipes a release asset into `tar` with no checksum, and that release is not marked immutable, so pinning the action does not fix the tool's bytes. |
| The release binary, with its checksum in the workflow | It fixes the bytes and skips the compile, for more shell, and it would also be moved by hand. |

What a new advisory does to pull requests that did not cause it:

| Option | Why not |
|---|---|
| **The check fails until someone decides** | Chosen. This project has one maintainer and few merges, so stopping costs one small pull request, and an advisory should not wait behind other work. |
| `continue-on-error` on advisories, as the vendor's README recommends | A red mark nobody has to act on. A pull request could also bring in the vulnerable crate itself and still merge. |

Where CodeQL's configuration lives:

| Option | Why not |
|---|---|
| **A workflow file** | Chosen. It is reviewed like the rest of the repository, and its actions are pinned like the others. |
| GitHub's default setup | One click and no file. It is a repository setting that a reader of the code cannot see or review. |

Whether CodeQL blocks a merge:

| Option | Why not |
|---|---|
| **It reports and does not block** | Chosen. Its first run on this code raised 9 `high` alerts and all 9 were false positives, so its record does not yet justify stopping a merge. |
| A required check | A false positive would stop a merge until someone dismissed it in GitHub's interface. Worth revisiting once it has a record on the server's code. |

Catching SQL built at run time, which CodeQL misses:

| Option | Why not |
|---|---|
| **`clippy`'s `disallowed-methods` on `AssertSqlSafe` and `QueryBuilder`'s constructors** | Chosen. sqlx accepts a string built at run time only through these, so the rule does not depend on tracing where the string came from. Nothing in the repository uses them. |
| `clippy`'s `disallowed-types` on `AssertSqlSafe` | The obvious rule, and it does not work. It flagged neither plant written `sqlx::AssertSqlSafe(sql)`, and of the others only the `use` line. |
| A CodeQL model for the wrapper | It would report only flows from a source CodeQL knows, and it adds a model pack to maintain. |
| Review alone | ADR-0006's rule already asks for that, and nothing enforced it. |

## Decision

- **A policy, `deny.toml`, checked by `cargo deny --locked check -D warnings`:**
  - **Advisories.** Every kind fails, on any crate that can be built: vulnerable,
    unmaintained, unsound or yanked. Yanked is included because a lock file keeps
    using a yanked version. To accept one, a reviewed `ignore` entry states why.
  - **Licences.** An allow list of the licences in use: `Apache-2.0`, `MIT`,
    `Unicode-3.0` and `Zlib`. Any other needs a decision.
  - **Sources.** crates.io and the sqlx repository of ADR-0006, nothing else.
  - **Bans.** Wildcard versions. No crate is banned by name: the one candidate,
    `time`, already fails the build (ADR-0022).
  - `-D warnings` turns an exception that matches nothing into a failure, so none
    outlives its reason.
  - Each of these was seen to fail: a vulnerable crate, a yanked version, an
    unmaintained crate, an unsound crate that is not a direct dependency, an
    unlisted licence, a wildcard version, a git source not allowed, and both kinds
    of leftover exception. A second registry was not tried.
- **A second run for the pinned crates.** `cargo-deny` matches an advisory only to
  a crate from the source it names, crates.io by default (`matches_rustsec` in its
  `src/advisories/helpers/db.rs`). A test advisory against `sqlx` fired for a
  crates.io copy and not for the pinned one, so the check alone could not see the
  crates that carry every query. CI therefore deletes the patch lines from its
  checkout and runs the advisories check again. The crates resolve to the same
  version from crates.io, so an advisory that names a version range still applies.
  The step then fails if any crate still comes from git: with one patch line in a
  format the `sed` missed, the tool printed `advisories ok` and left that crate
  pinned.
- **When it runs.** On every pull request, on every push to `main`, and daily,
  because an advisory can appear with no commit. `deny` joins the required checks.
- **Dependabot** for the actions and for the crates. Each gets one pull request a
  week, and `allow: dependency-type: all` includes the transitive crates, which
  are most of the tree and include `ctutils`. A release must be 7 days old to be
  proposed (GitHub's default is 3), which gives others time to find a bad one;
  security updates skip the wait. `sqlx*` is ignored, because those crates move by
  hand under ADR-0006; GitHub's page marks `ignore` as applying to security updates
  as well, and the second run above still reports an advisory against them.
- **Security updates and alerts** are turned on in the repository's settings.
- **CodeQL** for `rust` and `actions`, with the `security-extended` queries and no
  build. It runs on pull requests, on pushes to `main` and weekly. Its job alone
  holds a write permission, `security-events: write`, to upload results. It skips
  `crates/*/tests`: no code there ships, and 4 of the 9 false positives were test
  assertions there that print an ID when they fail, which `rust/cleartext-logging`
  reads as sensitive. On plants pushed to a branch that was never merged:

  | Planted | Reported |
  |---|---|
  | A pull request's title in a workflow's shell command | `actions/code-injection/critical` |
  | A checkout of a pull request's code under `pull_request_target` | `actions/untrusted-checkout/critical` |
  | A third-party action pinned to a tag | `actions/unpinned-tag` |
  | A password written to the output | `rust/cleartext-logging` |
  | A file path read from a socket | `rust/path-injection` |
  | SQL from a socket, through `query(AssertSqlSafe(..))` and `raw_sql(AssertSqlSafe(..))` | Nothing |
  | The same as `query(sql.as_str())`, which sqlx 0.9 no longer compiles | `rust/sql-injection` |

  The last two rows give the cause. CodeQL's model marks `query`'s first argument
  as the sink, and in sqlx 0.9 a run-time string arrives there inside a wrapper the
  model does not name.
- **SQL built at run time is a lint error.** `clippy.toml` names the wrapper's
  constructor and `QueryBuilder`'s two constructors. Plants through each of them
  were flagged, and without the rule the wrapper's were not. A call that needs one
  states why with `#[expect(clippy::disallowed_methods, reason = "...")]`, which
  keeps ADR-0006's escape hatch and makes using it visible.

## Consequences

- A new advisory can turn an unrelated pull request red. The fix is one small pull
  request: an update, or an `ignore` that states its reason.
- A scheduled failure notifies whoever last changed the cron line, and GitHub
  disables a schedule in a public repository after 60 days without activity. A
  quiet repository stops being watched, except by Dependabot's alerts.
- A new licence fails the check on purpose. The server will bring in `subtle`
  (ADR-0024), which is `BSD-3-Clause`, so that pull request adds it to the list.
- `cargo-deny`'s version is moved by hand, since Dependabot does not read a version
  in a workflow's `env`. An old version can stop understanding the advisory
  database, as when RustSec began using CVSS 4.0
  ([cargo-deny #804](https://github.com/EmbarkStudios/cargo-deny/issues/804)). That
  fails the check rather than passing it.
- While 0.9.1 is unreleased, the second run reads 0.9.0. Once it ships, that run
  reads 0.9.1 until the patch goes, and nothing announces the release, because
  Dependabot ignores those crates. Watching for it stays by hand, as ADR-0006 left it.
- GitHub's documentation does not say how Dependabot treats `[patch.crates-io]`. It
  can only be seen once this configuration is on `main`.
- `cargo-deny` skips the 12 lock entries behind features nothing enables, such as
  sqlx's SQLite driver, because no build contains them. Dependabot reads the lock
  file, so it can raise an alert on one, which would be dismissed for that reason.
- The other 5 false positives are in `#[cfg(test)]` code in `src/ids.rs`, which no
  path can exclude. They are dismissed as used in tests, and that decision lives in
  GitHub, not in this repository. `rust/cleartext-logging` also reads values named
  like `account` and `uuid` as sensitive, so the server's logs will raise alerts to
  read and dismiss.
- On a pull request, CodeQL reports only what that pull request changed. This one's
  check read "No new alerts in code changed by this pull request", with 0 results,
  where a push of the same code gave those 5. So a green check says nothing about
  older code: a finding there, or from a new query, appears when `main` is scanned,
  on the push or on the weekly run.
- The plants used a socket as their source, because axum is not a dependency yet.
  CodeQL lists axum as supported, and that stays unverified until the server's pull
  request plants one through a handler.
- `disallowed-methods` catching a tuple-struct constructor was measured, on `clippy`
  0.1.99, and is not in its documentation. An update could change it. The rule also
  guards against a mistake, not intent: a string leaked to `&'static str` passes.
- Visitors see less than the owner. Viewing the alert summary needs write
  permission, while anyone who can read the repository sees annotations on pull
  requests. What a reviewer can inspect is this record, the workflows and the checks.
- These checks read metadata and patterns. They do not establish that a dependency
  is honest, and a static analysis finding nothing proves nothing. Nothing here
  reviews the source of a dependency (`cargo-vet`) or attests how a build was made,
  `sqlx-cli` in CI has no lock file (ADR-0006), and scanning a running server or an
  image waits until there is a server and an image.

## Amendments

- 2026-11-04: Dependabot's first run on `main` shows how it treats
  `[patch.crates-io]`, which the Consequences left open. Its Cargo parser drops every
  crate named in `[patch]` (`file_parser.rb`, under `# TODO: Handle patched
  dependencies`), so `sqlx`, `sqlx-core`, `sqlx-macros`, `sqlx-macros-core` and
  `sqlx-postgres` are absent from the run's dependency snapshot, whatever the `ignore`
  says. The `ignore` stops only `sqlx-mysql` and `sqlx-sqlite`, which the lock file
  takes from the same git commit although the manifest does not patch them. Moving to
  0.9.1 also removes the `ignore`: left behind, it would stop every sqlx update, and
  nothing would show that. GitHub's dependency graph lists all 7 crates at 0.9.0, so
  an advisory on one should raise an alert with no fix pull request possible. That is
  unverified, because no such advisory exists to test with; the second `cargo-deny`
  run covers the case regardless.
- 2026-11-04: The 5 false positives in `src/ids.rs` are gone. Each alert pointed at
  the `"{uuid}"` message of an assertion and not at the values it compares, and adding
  `ApiKeyId` raised a sixth. The messages added nothing, because a failing `assert_eq!`
  prints the same UUID, so they were removed, and a new ID type raises no alert. The
  rest of that consequence stands: values named like `uuid` still read as sensitive,
  so the server's logs will raise alerts to read.
- 2026-11-05: Planted through axum handlers, as the Consequences asked, on a draft pull
  request closed unmerged, [#42](https://github.com/NikTheBoss97/payments-core/pull/42), with
  CodeQL 2.27.4. Its model of axum marks a handler's parameters as remote sources, and that
  holds: it does see axum. What it follows from there is narrower.

  | Planted in a handler | Reported |
  |---|---|
  | SQL from a path, as `query(sql.as_str())`, which sqlx 0.9 no longer compiles | `rust/sql-injection` |
  | SQL from a path, through `query(AssertSqlSafe(..))` | Nothing |
  | A file path from a path, or a `String` body | `rust/path-injection` |
  | A file path from a field or a method of an extractor of this project's own | `rust/path-injection` |
  | A file path from `Bytes`, through `String::from_utf8_lossy`, with axum's extractor or `KeyedRequest` | Nothing |
  | A file path from JSON parsed with `serde_json` | Nothing |
  | A credential from a header, or a new `ApiKeySecret`, with `println!` | `rust/cleartext-logging` |
  | The same two, with `tracing::error!` | Nothing |

  So a clean scan says little about this server. Every route reads its body raw and parses it
  with `serde_json`, where the taint stops, and the model list at that release has no
  `tracing`, the logger that the server uses, so the alerts on its logs that the entries above
  expected do not arise. What stands behind this is not CodeQL: SQL goes through checked macros
  and `clippy` refuses the wrapper, a secret has no `Display` and a `Debug` that hides it, a log
  line holds an error's chain and nothing of the request, and tests on the real binary assert
  that its log holds no credential. CodeQL still sees an ID from a path, and SQL or a secret
  written in the forms it models.

## References

- Rust Secure Code Working Group, [RustSec Advisory Database](https://rustsec.org/).
- Embark Studios, [cargo-deny](https://embarkstudios.github.io/cargo-deny/), and its action's [Dockerfile at v2.1.1](https://github.com/EmbarkStudios/cargo-deny-action/blob/v2.1.1/Dockerfile).
- The Cargo Book, [Publishing on crates.io](https://doc.rust-lang.org/cargo/reference/publishing.html) ("The version can never be overwritten") and [`cargo install`](https://doc.rust-lang.org/cargo/commands/cargo-install.html) (`--locked`).
- GitHub Docs, [About Dependabot alerts](https://docs.github.com/en/code-security/dependabot/dependabot-alerts/about-dependabot-alerts), [About the GitHub Advisory Database](https://docs.github.com/en/code-security/security-advisories/working-with-global-security-advisories-from-the-github-advisory-database/about-the-github-advisory-database), [Dependabot options reference](https://docs.github.com/en/code-security/reference/supply-chain-security/dependabot-options-reference) (`cooldown`, `ignore`, `allow`), [About code scanning with CodeQL](https://docs.github.com/en/code-security/code-scanning/introduction-to-code-scanning/about-code-scanning-with-codeql), [Customizing your advanced setup](https://docs.github.com/en/code-security/code-scanning/creating-an-advanced-setup-for-code-scanning/customizing-your-advanced-setup-for-code-scanning) (`paths-ignore`) and [Events that trigger workflows](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows) (`schedule`).
- dependabot-core `6a43f5c`: [cargo/lib/dependabot/cargo/file_parser.rb](https://github.com/dependabot/dependabot-core/blob/6a43f5c18a7120386064c4afabb6a3e803d63d90/cargo/lib/dependabot/cargo/file_parser.rb#L39-L40) (`# TODO: Handle patched dependencies`).
- CodeQL, [Supported languages and frameworks](https://codeql.github.com/docs/codeql-overview/supported-languages-and-frameworks/), and at CLI 2.27.4 its [model of sqlx](https://github.com/github/codeql/blob/codeql-cli/v2.27.4/rust/ql/lib/codeql/rust/frameworks/sqlx.model.yml), its [model of axum](https://github.com/github/codeql/blob/codeql-cli/v2.27.4/rust/ql/lib/codeql/rust/frameworks/axum.model.yml) and its [list of models](https://github.com/github/codeql/tree/codeql-cli/v2.27.4/rust/ql/lib/codeql/rust/frameworks).
- sqlx `f1e94ec`: [sqlx-core/src/sql_str.rs](https://github.com/transact-rs/sqlx/blob/f1e94ec34dd5f11ad6488a3a8ac7988439a3f757/sqlx-core/src/sql_str.rs) (`SqlSafeStr` and `AssertSqlSafe`).
- Clippy, [Lint configuration](https://doc.rust-lang.org/clippy/lint_configuration.html) (`disallowed-methods`, `disallowed-types`).
- Piergiorgio Ladisa et al., [SoK: Taxonomy of Attacks on Open-Source Software Supply Chains](https://doi.org/10.1109/SP46215.2023.10179304) (IEEE S&P, 2023).
- William Woodruff, [We should all be using dependency cooldowns](https://blog.yossarian.net/2025/11/21/We-should-all-be-using-dependency-cooldowns) (2025).
- Wachiraphan Charoenwet et al., [An Empirical Study of Static Analysis Tools for Secure Code Review](https://doi.org/10.1145/3650212.3680313) (ISSTA, 2024).
- Runzhi He et al., [Automating Dependency Updates in Practice: An Exploratory Study on GitHub Dependabot](https://doi.org/10.1109/TSE.2023.3278129) (IEEE Transactions on Software Engineering, 2023).
- Hao Li et al., [An Empirical Study of Yanked Releases in the Rust Package Registry](https://doi.org/10.1109/TSE.2022.3152148) (IEEE Transactions on Software Engineering, 2023).
- Nasif Imtiaz et al., [A Comparative Study of Vulnerability Reporting by Software Composition Analysis Tools](https://doi.org/10.1145/3475716.3475769) (ESEM, 2021).
- Pavel Avgustinov et al., [QL: Object-oriented Queries on Relational Data](https://doi.org/10.4230/LIPIcs.ECOOP.2016.2) (ECOOP, 2016).
