---
description: "Use when working on mssql-py-core async pool types, configuration, ownership, deadlines, cancellation or Deadpool fit tests."
applyTo: "mssql-py-core/src/pool.rs,mssql-py-core/src/pool/**"
---

# Async pool context

- Read the [module context guide](../../mssql-py-core/src/pool/README.md) before
  changing pool contracts or tests. It maps responsibilities, ownership and
  contract boundaries without requiring generated API documentation.
- Use the linked pool, slot-adaptation and Python integration designs for
  behavioral requirements. Distinguish compiled foundation contracts and the
  test-only fit experiment from later production integration.
- Keep the context guide's type map, scope and ownership rules
  accurate when responsibilities or ownership change.
