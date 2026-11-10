# Bot research workbench

Implementation plan registered before running new experiments. Existing registered studies
and their recorded results remain unchanged. New simulation results describe a synthetic
market, not a live trading edge. Candidate forecasting models stay in research until an
independent evaluation justifies promotion.

## Deliverables

- [x] Reproducible experiment runner: immutable manifests, source/data fingerprints, common
  random inputs, per-seed results, paired comparisons, portable reports.
- [x] Order safety: persistent client order IDs, uncertain-result reconciliation, post-only
  quotes, startup/shutdown cancellation, server-enforced heartbeat expiry.
- [x] Risk: per-market, per-event, category and account exposure budgets including resting
  orders; reducing exposure remains possible when a budget is exceeded.
- [x] Forecast research: timestamp-validated archived observations, chronological evaluation,
  calibration and baseline comparisons, candidate ensemble/conditional weather models.
- [x] Stress tests: informed flow, shocks, thin liquidity, latency, stale feeds and outages.
- [x] Research dashboard: experiment comparison, trade/quote explanations, model evidence and
  live maker metrics, without exposing credentials or introducing public trading controls.
- [x] Historical replay: recorded commands and events verified against the reference engine,
  plus a step-through viewer; hypothetical fills labelled as simulated.
- [x] Repair the demonstrated market-switch, Sell/Limit, listing-retry and void-copy defects.
- [x] Document one-command research setup, repeatable checks and the limits of the results.

## Evaluation registered before results

1. Identical configuration, source and seed must reproduce the same tape and result hashes.
2. Each strategy in a paired comparison must receive identical exogenous paths and arrivals.
3. Heartbeat expiry and stale-feed scenarios must withdraw quotes within their configured
   deadlines. Pending order acknowledgements must not create a second order on retry.
4. Risk-increasing quotes must fit their configured budgets after counting existing exposure
   and all potentially filled resting orders. Tests include related outcomes and short YES.
5. Forecast evaluation must use only inputs and training outcomes available at prediction
   time. Report sample sizes, calibration, Brier score and paired baseline differences.
6. No forecasting candidate is promoted by its in-sample score, a handpicked seed, or a
   synthetic-data demonstration. A held-out result and its dataset hash are required.

## Initial experiment design

Compare the registered reference quoting parameters with a conservative candidate on the
same seeds under noise, informed flow, a mid-session information shock, thin flow, delayed
orders, stale data, and a disconnected maker. Report all conditions, including regressions.
Use a small seed set for executable examples; reserve disjoint seeds for evaluation. Report
settled P&L, marked-equity drawdown, inventory, fills, spread/quote availability, rejected
orders and stale-quote exposure. Mark-to-model equity is diagnostic, not liquidation value.

The dashboard and replay are local research artifacts. Opening them must not place an order
or send data to a third party. A production gateway remains the authority for live orders.

## Implementation and validation record

Implemented against `fc4e8cc310971780b7ed66bceccc658faa60dd34`. The work includes the
seven research/bot priorities, the four reproduced defects, and a dependency-build fix:
`uv_build==0.13.5` is available before the lock's dependency-age cutoff, unlike the former
minimum. No new runtime package dependency was added.

Validation completed in a Linux development workspace:

- Rust: all 134 workspace tests passed, including the otherwise ignored PostgreSQL tests.
  Workspace formatting and Clippy with warnings denied passed. After the final lease changes,
  REST, WebSocket, API-contract and lease tests were rerun; the lease audit test also verifies
  that automatic expiry is attributed to the system. Initial lease checks used PostgreSQL 16;
  the full workspace run used PostgreSQL 17 after a workspace restart.
- Python: all 256 tests in the full suite passed, including live-client, differential,
  crash/recovery and catalogue integration tests. A subsequently added credential-redaction
  regression also passed; the final fast suite contains 248 passing tests. Ruff formatting,
  lint and strict mypy passed. Tests cover lost acknowledgements across a process restart,
  state-file locking and identity, correlated exposure, stale data, immutable artifacts,
  forecast look-ahead, incomplete listings, public recording gaps and the monitor's boundary.
- Website: production build, TypeScript and ESLint passed. Ten complete exchange/browser
  flows passed, with accessibility checks, including Sell/Limit and delayed old-socket-close
  regressions. Two separate report-browser tests passed, including accessibility, keyboard
  replay controls, no remote resource requests, and hiding an incomplete recorded book.
- Reports: desktop and 390-pixel mobile layouts were inspected, with no horizontal page
  overflow or JavaScript errors. The synthetic forecast report rendered successfully.
- Research: the five-seed development demonstration produced 70 sessions. Every stored
  command/event trace replayed exactly. A separate evaluation on seeds 1001–1020 produced
  another 280 sessions, all verified and replayed. No strategy was tuned between these runs.

## Held-out synthetic evaluation

Run `96826866840e39fc8def`, source snapshot
`367dcde387432c5f09a73aedd34e261500892e2980783b39e686ba5973c14cc1`.
Reproduce the configuration with `research run --start-seed 1001 --seeds 20`.
The run ID also incorporates source/environment provenance, so another checkout can have a
new ID. Full inputs, source and results are retained together in the generated artifact.

Conservative minus reference, in play credits; 20 paired seeds per scenario:

| Scenario | Mean settled profit difference | 95% seed-bootstrap interval | Mean drawdown difference |
|---|---:|---:|---:|
| Disconnect | +5.32 | -20.66 to +29.36 | -30.57 |
| Informed | +8.41 | -23.50 to +40.47 | -37.23 |
| Latency | -10.04 | -35.02 to +15.71 | -26.57 |
| Noise | +3.66 | -24.14 to +34.61 | -31.20 |
| Shock | -17.73 | -46.41 to +8.01 | -24.59 |
| Stale feed | +7.24 | -19.33 to +32.43 | -31.60 |
| Thin | +3.20 | -10.34 to +15.69 | -17.63 |

Every profit interval includes zero. The smaller candidate reduced marked drawdown in these
simulations but did not establish better profitability. Quotes remained absent after the
configured stale/heartbeat deadlines (zero stale-quote steps). These findings do not establish
live performance, and no candidate was promoted.

## Evidence still needed before model promotion

The software deliverables above are implemented. An independently provenance-checked observed
forecast archive, fixed evaluation criteria, and out-of-sample results are still needed to
justify replacing live weather models. Ensemble output in the example is explicitly synthetic.
The new guarded gateway path also needs a fresh load/latency study before applying the old
throughput claims to it. Multi-gateway active/active lease coordination is outside this version.
Observed public market replay does not recover hidden queues or counterfactual fills.

Usage and Windows commands: [research workbench guide](../research-workbench.md).
