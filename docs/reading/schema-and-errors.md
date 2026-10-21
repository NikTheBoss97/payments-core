<!-- Author: Nicola Antonello -->

# Schema design and database errors

Constraints, privileges and operator resolution in PostgreSQL, and error codes that programs can act on. Part of the [reading list](README.md).

- ★ PostgreSQL, [5.5. Constraints](https://www.postgresql.org/docs/18/ddl-constraints.html) (PostgreSQL 18 docs). What each kind of constraint can check, why a foreign key must target a unique key (which lets a child row repeat its parent's values), and how NULLs slip past unique and check constraints.
- PostgreSQL, [5.8. Privileges](https://www.postgresql.org/docs/18/ddl-priv.html) (PostgreSQL 18 docs). Privileges granted per column, which leave a column such as a recording time to its default.
- PostgreSQL, [10.2. Operators](https://www.postgresql.org/docs/18/typeconv-oper.html) (PostgreSQL 18 docs). How PostgreSQL picks an operator: one that takes exactly the argument types wins wherever it sits on the search path, so a guard function compares only types for which `pg_catalog` has an exact operator.
- PostgreSQL wiki, [A Guide to CVE-2018-1058: Protect Your Search Path](https://wiki.postgresql.org/wiki/A_Guide_to_CVE-2018-1058:_Protect_Your_Search_Path). How a function created in `public` takes over calls that a built-in would answer through a cast, and the defences: no `CREATE` on `public`, and a narrower `search_path`.
- Bill Karwin, [SQL Antipatterns, Volume 1: Avoiding the Pitfalls of Database Programming](https://pragprog.com/titles/bksap1/sql-antipatterns-volume-1/) (Pragmatic Bookshelf, 2022). Includes Polymorphic Associations: why no foreign key can check a `(type, id)` pair, and what to do instead.
- Craig Kerstiens, [Enums vs Check Constraints in Postgres](https://www.crunchydata.com/blog/enums-vs-check-constraints-in-postgres) (Crunchy Data, 2022). A short case for a `CHECK` constraint over an enum type for a closed set of values.
- PostgreSQL, [Appendix A. PostgreSQL Error Codes](https://www.postgresql.org/docs/18/errcodes-appendix.html) (PostgreSQL 18 docs). Five-character SQLSTATEs, whose first two characters name a class that a program can act on without knowing the exact code.
- PostgreSQL, [41.9. Errors and Messages](https://www.postgresql.org/docs/18/plpgsql-errors-and-messages.html) (PostgreSQL 18 docs). Raising errors with a custom SQLSTATE from PL/pgSQL, and why codes ending in 000 are categories.
- Google, [AIP-193: Errors](https://google.aip.dev/193) (API Improvement Proposals). Messages are for people and machine-readable reasons are for programs, so clients never need to parse a message.
