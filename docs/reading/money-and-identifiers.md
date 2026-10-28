<!-- Author: Nicola Antonello -->

# Money and identifiers

Representing amounts and currencies exactly, splitting them without losing a unit, and generating identifiers. Part of the [reading list](README.md).

- ★ K. Davis, B. Peabody and P. Leach, [RFC 9562: Universally Unique IDentifiers (UUIDs)](https://www.rfc-editor.org/rfc/rfc9562.html) (IETF, 2024). Defines UUIDv7, and explains why random UUIDs hurt B-tree index locality.
- ★ Martin Fowler, [Money](https://martinfowler.com/eaaCatalog/money.html) (Patterns of Enterprise Application Architecture, 2002). Keep an amount with its currency, and watch for lost pennies when rounding to minor units.
- [ISO 4217 currency codes](https://www.iso.org/iso-4217-currency-codes.html) (ISO). The three-letter codes, and how many minor units each currency has (not always two).
- Stripe, [Supported currencies](https://docs.stripe.com/currencies). How a real PSP sends amounts as integer minor units, and where it departs from ISO 4217.
- PostgreSQL wiki, [Don't use money](https://wiki.postgresql.org/wiki/Don%27t_Do_This#Don.27t_use_money). Why the `money` type is a poor fit: it stores no currency, takes it from the locale, and rounds in surprising ways.
- Alex Petrov, [Database Internals](https://www.oreilly.com/library/view/database-internals/9781492040330/) (O'Reilly, 2019). Chapter 4 covers how B-tree pages split, and the right-only appends optimization for keys that only grow, the pattern UUIDv7 keys follow.
- Jean-Michel Muller et al., [Handbook of Floating-Point Arithmetic, 2nd edition](https://doi.org/10.1007/978-3-319-76526-6) (Birkhäuser, 2018). The current reference on floating-point formats, including IEEE 754-2008, which merged the binary standard with the radix-independent IEEE 854 and its decimal formats.
- Friedrich Pukelsheim, [Proportional Representation: Apportionment Methods and Their Applications, 2nd edition](https://doi.org/10.1007/978-3-319-64707-4) (Springer, 2017). Chapter 5's quota methods ("divide and rank", led by the Hare-quota method) are the theory behind the allocation function that splits a total into whole minor units without losing one.
- Michael F. Cowlishaw, [Decimal Floating-Point: Algorism for Computers](https://doi.org/10.1109/ARITH.2003.1207666) (IEEE Symposium on Computer Arithmetic, 2003). Why commercial arithmetic must be decimal (a 5% tax on a $0.70 call rounds to $0.73 in binary but $0.74 by hand), and the scaled integer (2.50 held as 250 with a scale of 2) that the project's minor units fix per currency.
- David Goldberg, [What Every Computer Scientist Should Know About Floating-Point Arithmetic](https://doi.org/10.1145/103162.103163) (ACM Computing Surveys, 1991). Why decimal fractions such as 0.1 have no exact binary representation.
