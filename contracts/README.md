# EMP query projections

This directory holds production read-only SQL projections compiled into
`emp-state`:

- [call-outcomes.sql](call-outcomes.sql) classifies call outcomes from stored
  request facts and delivery evidence without rewriting records.
- [usage-owners.sql](usage-owners.sql) resolves historical usage to an account
  only when matching realtime records identify one unique account fingerprint.

State compatibility and normalization fixtures live in
[emp-state/tests/data/](../crates/emp-state/tests/data/). Rust integration tests
load them directly with `include_str!`, without a Python process or checkout.
The historical `python-` filenames identify the origin of frozen inputs and
expected results. The crypto fixture covers legacy vault and migration files
that must remain readable.
