<!-- Author: Nicola Antonello -->

# Money and identifiers

Representing amounts and currencies exactly, and generating identifiers. Part of the [reading list](README.md).

- ★ K. Davis, B. Peabody and P. Leach, [RFC 9562: Universally Unique IDentifiers (UUIDs)](https://www.rfc-editor.org/rfc/rfc9562.html) (IETF, 2024). Defines UUIDv7, and explains why random UUIDs hurt B-tree index locality.
- ★ Martin Fowler, [Money](https://martinfowler.com/eaaCatalog/money.html) (Patterns of Enterprise Application Architecture, 2002). Keep an amount with its currency, and watch for lost pennies when rounding to minor units.
- [ISO 4217 currency codes](https://www.iso.org/iso-4217-currency-codes.html) (ISO). The three-letter codes, and how many minor units each currency has (not always two).
- Stripe, [Supported currencies](https://docs.stripe.com/currencies). How a real PSP sends amounts as integer minor units, and where it departs from ISO 4217.
- PostgreSQL wiki, [Don't use money](https://wiki.postgresql.org/wiki/Don%27t_Do_This#Don.27t_use_money). Why the `money` type is a poor fit: it stores no currency, takes it from the locale, and rounds in surprising ways.
- David Goldberg, [What Every Computer Scientist Should Know About Floating-Point Arithmetic](https://doi.org/10.1145/103162.103163) (ACM Computing Surveys, 1991). Why decimal fractions such as 0.1 have no exact binary representation.
