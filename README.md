# Brier

A prediction-market exchange, named after the Brier score (Brier, 1950), the standard measure of how good probability forecasts are. It is built to the engineering standard of a regulated venue, and run with play money.

Traders buy and sell yes/no contracts on things public data settles: tomorrow's high in twelve US cities, how many strong earthquakes strike today, whether a geomagnetic storm arrives, inflation, jobs, and rates. A recurring catalogue lists the next two days of every series by itself, grouped into events like "Highest temperature in Denver on Nov 12?". A contract pays 100 if the event happens and 0 if it does not, so its price (1 to 99) reads as the market's probability. Under that simple product sits the machinery a real exchange needs:

- **Deterministic matching engine.** Price-time priority order book. Replaying the command log reproduces every state hash exactly.
- **Double-entry ledger.** Fully collateralized contracts and pre-trade collateral checks. A conservation invariant is checked after every event: credits issued = balances + escrow.
- **Crash-safe journal.** No order is acknowledged before it is on disk (one full flush per batch of orders), and recovery is proven by killing the core 1,000 times under load.
- **APIs.** REST and WebSocket order entry with signed requests and idempotent retries; sequenced market data with gap recovery.
- **Evidence-backed resolution.**
  - Every market has a rulebook whose hash is in the journal.
  - It resolves from free public sources: the National Weather Service, USGS, NOAA, BLS, and FRED.
  - It settles only after a second check: a second admin for monthly markets, or, for daily ones, an independent re-fetch that must agree. A dispute window follows.
  - The settlement names its evidence by hash ([docs/resolution.md](docs/resolution.md)).
- **Reference market maker.**
  - It trades only through the public API.
  - It quotes around fair values from models backtested on history they were not fitted on ([docs/notes/catalog-models.md](docs/notes/catalog-models.md)).
  - It does not quote at all where no defensible model exists.
  - Durable order intents, post-only quotes, account-scoped cancellation leases and exposure
    limits protect unattended quoting. Use a dedicated account and persistent state file.
- **Research workbench.** Reproducible stress experiments, paired strategy comparisons,
  chronological weather evaluation, offline replay and a local bot decision monitor.
  [Start here, including Windows instructions](docs/research-workbench.md).
- **Market surveillance.** Detects wash trading, spoofing, marking the close, and insider-like trading. Precision and recall are measured on simulated manipulation.

The full standard is [docs/SPEC.md](docs/SPEC.md). The exact engine behavior is in [docs/protocol.md](docs/protocol.md). Design decisions are in [docs/adr/](docs/adr/).

## Play money, by design

Credits are granted at signup and through the daily allowance. They cannot be bought, sold, withdrawn, redeemed, or transferred, and they have no cash value. Real-money event contracts for US persons require a CFTC-designated contract market, which this project is not. The code contains no deposit, withdrawal, or payment path, and a test will enforce that from G2 onward.

## Status

Built in gates G0 to G10 ([SPEC §15](docs/SPEC.md)). Nothing is released before every gate passes.

| Gate | Scope | State |
|---|---|---|
| G0 | Toolchains, domain types, build, CI, security scans | Done |
| G1 | Matching engine and independent reference engine | Done |
| G2 | Ledger, collateral, settlement | Done |
| G3 | Journal, snapshots, crash recovery | Done |
| G4 | Gateway, APIs, PostgreSQL read models | Done |
| G5 | Markets, rulebook, resolution | Done |
| G6 | Web app | Done |
| G7 | Market maker, simulation harness | Done |
| G8 | Surveillance and its evaluation | Done |
| G9 | Live catalogue: events, recurring series, a Polymarket-grade interface | Next |
| G10 | Release | |

## Measured so far

| What | Result | Evidence |
|---|---|---|
| Matching throughput, one core | 12.3 million commands per second (median of 5 runs) | [performance note](docs/notes/performance.md) |
| Matching latency per command | p99 334 ns, p99.9 500 ns (timer resolution 42 ns) | [performance note](docs/notes/performance.md) |
| Agreement with an independent reference engine | 10 million random commands, 12.4 million events, 0 divergences (matching only) | [differential note](docs/notes/differential.md) |
| The same, with the ledger | 10 million state-aware commands with accounts, collateral, settlement, and kill switches: 19.6 million events, 696,614 trades, 0 divergences, full invariant checks every 100,000 commands | [differential note](docs/notes/differential.md) |
| Throughput with the ledger in the order path | 2.69 million commands per second; p99 1.4 µs (65% places, 35% cancels; 38% of places rejected) | [performance note](docs/notes/performance.md) |
| Crash safety | 1,000 SIGKILLs under load with simulated torn writes: 0 of 6,058,014 acknowledged commands lost; every recovered state equal to a clean replay | [durability note](docs/notes/durability.md) |
| Durable sync cost | 3.95 ms median per batch (full device flush, `F_FULLFSYNC`); group commit spreads it across every order in the batch | [durability note](docs/notes/durability.md) |
| Recovery replay | 10 million commands from disk in 7.7 s (median of 3) | [durability note](docs/notes/durability.md) |
| Order to acknowledgment through the gateway | p50 4.74 ms, p99 9.14 ms at 10,000 orders/s (one to two durable syncs); the 5 ms p99 target was refuted, the registered 9.9 ms bound held | [gateway note](docs/notes/gateway.md) |
| Market data to 1,000 WebSocket subscribers | p50 3.69 ms, p99 16.4 ms after batching writes (was 22.9 ms); the 10 ms p99 target was refuted, remaining tail is intermittent machine-wide stalls | [gateway note](docs/notes/gateway.md) |
| Fuzzing | 6 targets (wire, journal decode and recovery, snapshot restore, gateway requests, signatures), 1 hour each, about 2.2 billion executions; one finding (non-canonical snapshot bytes), fixed | [G4 review](docs/gates/G4-review.md) |
| Resolver accuracy | 855 values from BLS, FRED, and NOAA equal to independent comparators (FRED, Treasury, FOMC statements, GHCN-Daily), 0 mismatches; every decision recomputed identically from the stored response with networking disabled | [resolution note](docs/notes/resolution.md) |
| Resolution end to end | All five launch markets listed, traded, closed by schedule, resolved, disputed, and settled through a real gateway | [G5 review](docs/gates/G5-review.md) |
| Web app end to end | 6 browser flows (sign-up with TOTP, trading, forecasting, admin confirmation and dispute ruling, settlement, scorecard) against a real exchange; no WCAG 2.2 AA violations on 9 pages (axe-core) | [G6 review](docs/gates/G6-review.md) |
| Reference market maker | Registered study passed: +69.73 credits per market against noise (t = 12.9), losses rising with traders' information (80.25 → 71.78 → 54.77), inventory within limits in 800 simulated markets; 100% two-sided uptime on four live launch markets | [market-maker note](docs/notes/market-maker.md) |
| Market surveillance | Pre-registered, thresholds tuned on seeds 1-100 and reported on 101-300: wash trading precision 1.00, recall 0.98; insider-like trading 1.00 and 0.99; spoofing (precision 0.30) and marking the close (0.54) did not meet their targets, reported as failures | [surveillance note](docs/notes/surveillance.md) |
| Catalogue pricing models | Pre-registered backtest: the temperature model beats 30-year climatology in all 12 cities and is calibrated within 0.05; the earthquake count models failed their hypotheses, reported as failures (outcome frequencies, exploratory, barely beat uniform) | [catalogue-models note](docs/notes/catalog-models.md) |
| Settlement source check | NWS daily climate reports matched NOAA GHCN-Daily on 53 of 53 days | [catalogue-models note](docs/notes/catalog-models.md) |
| Book and ledger invariants (I1-I5) | Checked after every command in 2,560 random streams per CI run | `crates/engine/tests/properties.rs` |

Measured on a 16 GB Apple M5 MacBook Air on AC power. Both performance hypotheses were registered before the engine existed.

## Run the checks

Requirements: [Rust via rustup](https://rustup.rs) (the pinned toolchain installs itself from `rust-toolchain.toml`), [uv](https://docs.astral.sh/uv/), [pre-commit](https://pre-commit.com/), and [gitleaks](https://github.com/gitleaks/gitleaks).

```bash
make setup   # locked Python dependencies and git hooks
make check   # Rust fmt, clippy (warnings are errors), tests; Python ruff, mypy --strict, tests
make audit   # known-vulnerability audit of locked dependencies (needs cargo-audit)
make bench   # the registered engine benchmark
make differential-full SEED=1   # 10 million commands through both engines
make crash SEED=1               # 1,000 SIGKILLs against brier-core with recovery checks
make help    # every command
```

## How it is built

| Part | Technology |
|---|---|
| Order path: engine, ledger, journal, gateway | Rust ([ADR-0001](docs/adr/0001-languages-and-architecture.md)) |
| Resolvers, market maker, surveillance, simulation, reference engine | Python 3.12 |
| Web app (G6) | Next.js, TypeScript |
| Read models (G4) | PostgreSQL 16 |

Library code may not panic, unwrap, use floating point, or do unchecked integer arithmetic. These are compiler lints that fail the build, not conventions.

## License

All rights reserved. The code is published to be read, not reused: no license is granted to copy, modify, run, or redistribute it, and contributions are not accepted. See [LICENSE](LICENSE).
