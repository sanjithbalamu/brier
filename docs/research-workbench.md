# Research workbench

The workbench separates strategy experiments, forecast evaluation and observed market replay.
It produces local files that can be reviewed without an account or network connection.
Research commands do not start a trading bot. The existing registered studies and their
results remain in `docs/notes/`; these new tools do not rewrite those results.

## Start on Windows

Install [uv](https://docs.astral.sh/uv/getting-started/installation/) if needed, then open
PowerShell in the repository folder (for example, `C:\Users\reach\brier`). Run:

```powershell
uv sync --project services --locked
uv run --project services --locked python -m brier_services.research run --seeds 5
```

The command prints a run directory and an HTML filename under `var/research`. Open that HTML
file in your browser. To open the newest report from PowerShell:

```powershell
Get-ChildItem .\var\research\*.html | Sort-Object LastWriteTime -Descending | Select-Object -First 1 | Invoke-Item
```

Docker, PostgreSQL and the website server are not needed for these experiments. The HTML file
works offline. On macOS/Linux, the same `uv` commands work; `make research` runs the example
configuration. Generated data is git-ignored under `var/`.

## Design and compare experiments

```text
uv run --project services --locked python -m brier_services.research run --config research/stress.example.json
```

Copy the example JSON to register your experiment before running it. The first strategy is
the comparison baseline. Each scenario may inherit a `base` preset and override its name,
length, latency, signal quality or outage bounds. Each strategy may override `params` (size,
inventory limit, skew and spread parameters), requote frequency and safety deadlines. Unknown
fields, non-finite values and invalid types fail validation. No Python code is loaded from
configuration. Names must use lowercase letters, digits, `_` or `-` and be unique in each group.

Preset scenarios cover noise, informed flow, an information shock, thin flow, delayed orders,
stale forecasts and a disconnected maker. A session uses 480 abstract steps by default; steps
are not milliseconds. All exogenous information, trader arrival intentions and outcomes are
generated before any strategy runs. Strategies receive identical tapes for the same scenario
and seed. They can produce different fills because their quotes differ.

Start with seeds 1–5 for development. Fix the candidate and criteria, then use disjoint seeds
(for example `--start-seed 1001 --seeds 20`) for evaluation. Report every scenario, including
regressions. Repeatedly tuning against those held-out results makes them development data.

The dashboard provides paired profit/drawdown differences, seed bootstrap intervals, all
session results, a step-through quote/position/trade viewer and source provenance. One seed
does not receive an uncertainty interval. Small-sample intervals are exploratory and are not
adjusted for selecting among many candidates. Inventory is net YES; a negative number is NO.
100 ticks equal one play credit. Marked equity uses the model probability, not executable
liquidation prices. Final profit uses actual simulated settlement.

Each immutable run contains:

| File | Contents |
|---|---|
| `manifest.json` | Explicit configuration, seed list, source hash, commit, Python version and file hashes |
| `source.json.gz` | Python source, HTML templates, package metadata and lockfile |
| `tape-*.json.gz` | Common exogenous paths, arrivals and outcomes |
| `trace-*.json.gz` | Exact reference-engine commands, replies, decisions and metrics |
| `summary.json` | Every session and paired comparisons |

Repeated identical runs reuse verified artifacts. Changes to configuration or source create a
new directory. Writes are staged and published only after completion. Checksums detect edits;
they do not prove authorship. Floating-point reproduction should use the recorded Python
version and environment. To verify, replay or regenerate a report, replace `RUN_DIRECTORY`:

```text
uv run --project services --locked python -m brier_services.research verify RUN_DIRECTORY
uv run --project services --locked python -m brier_services.research replay RUN_DIRECTORY
uv run --project services --locked python -m brier_services.research report RUN_DIRECTORY --out var/report.html
uv run --project services --locked python -m brier_services.research compare BASE_DIRECTORY CANDIDATE_DIRECTORY --strategy reference
```

Cross-run comparisons require the same scenario/seed sets and exact tape hashes. No unmatched
seed is silently dropped. The simulator models latency and protective cancellation, but has
one maker, immediate protective cancels, no competing quote queues, no transaction fees and
no market impact model. It is a diagnostic stress environment, not a live-return forecast.

## Evaluate weather models chronologically

Try the complete workflow with an explicitly synthetic example:

```text
uv run --project services --locked python -m brier_services.research weather-example --out var/weather-example.json
uv run --project services --locked python -m brier_services.research evaluate-weather var/weather-example.json --test-start 2026-03-01 --out var/forecast-research
```

Open the printed `report.html`. The evaluator compares historical frequencies, a fixed-width
forecast, city bias correction, city/horizon/season calibration with fallbacks, and an ensemble
mixture. Model definitions are fixed before scoring. Fits use only outcomes resolved before
each prediction and within the configured rolling window (`--window-days`, default 365).
The minimum training count is configurable with `--min-train` (default 20). Missing ensembles
are reported as skipped; they are not reconstructed from future observations.

For actual evidence, supply a JSON object with `schema: 1`, `kind: "observed"`, a descriptive
`source`, and `records` matching the example. Every record requires:

| Field | Meaning |
|---|---|
| `id`, `city`, `target_date` | Unique prediction ID, stable city ID, local target date |
| `issued_at` | When this prediction was made, with explicit timezone |
| `available_at` | When all input forecasts were actually available; must not be later than issuance |
| `resolved_at` | When the contract's observed result became available; must follow issuance |
| `horizon_hours` | Forecast lead time, consistently defined by the dataset producer |
| `forecast_high`, `actual_high` | Forecast Fahrenheit high and whole-degree settlement reading |
| `ensemble` | Optional archived member highs available at issuance |
| `ranges` | Exhaustive, non-overlapping inclusive integer ranges, including both unbounded tails |

Preserve provider responses and collection/availability timestamps when building an observed
archive. An API's current reconstruction of historical weather is not proof of what was
available at an earlier forecast issuance. The evaluator validates supplied timestamps; it
cannot independently authenticate them. It does not download or fabricate an observed archive.

Reports include sample sizes, multiclass Brier scores, descriptive calibration bins,
city/horizon/season breakdowns and differences paired against historical frequencies.
Bootstrap intervals resample target days; their mean gives each day equal weight. Adjacent
days can remain correlated. Overall model scores may cover different examples when archives
are missing; use the paired comparisons and inspect counts. Synthetic scores establish only
that the workflow runs. No candidate is automatically promoted to the live maker.

## Record and replay observed market data

With your gateway running, record an existing market's public stream:

```text
uv run --project services --locked python -m brier_services.research record-market --ws ws://127.0.0.1:8080/v1/ws --market 101 --seconds 300 --out var/market-101.jsonl
uv run --project services --locked python -m brier_services.research history-report var/market-101.jsonl --out var/market-101.html
```

Use your gateway's actual port and market ID. The recorder subscribes anonymously to that
market's book and trades, flushes each message, and requests a fresh subscription after a
sequence gap. It cannot send orders or access private account channels. A disconnected
session ends; start a new file to record again. Existing files are never overwritten.

The offline viewer reconstructs only sequence-complete books, hides the book during gaps,
and shows only trades received up to the selected message. The replay includes a hash of the
recording. A truncated final JSON line is an error, not silently treated as complete data.
This is observed state, not a strategy backtest: queue position and hypothetical fills cannot
be recovered from public top-level market data alone.

## Run and inspect the protected maker

The gateway applies migration `0014_order_leases.sql` through its normal startup migrations.
The new maker CLI requires that gateway version. Use one dedicated bot account, one running
maker and one persistent SQLite state file per account/gateway. Other trading on that same
account can invalidate client-side exposure assumptions or evict old idempotency keys.

After registering an API key using the existing account/key workflow:

```text
uv run --project services --locked python -m brier_services.marketmaker run --gateway http://127.0.0.1:8080 --key-id 1 --key-file var/maker.key --state var/marketmaker/orders.sqlite --size 10 --market-limit 150 --event-limit 300 --category-limit 1000 --total-limit 2000 --lease-seconds 60
```

Use the actual endpoint, key ID and key file. The CLI stores each post-only GTC order intent
before transmitting it. A lost acknowledgement is retried with exactly the same client order
ID and body before any new order. The state file is locked to one process and bound to the
gateway/account identity. Keep it across restarts; do not delete a pending intent to bypass an
uncertain result. Start-up and graceful shutdown cancel the dedicated account's resting orders.

Exposure counts current positions and all potentially filled resting bids/offers. Budgets
apply per market, event, category and account in maximum-payout contract-equivalents, not cash
or estimated value at risk. No hedging credit is assumed across correlated outcomes. Orders
that do not increase worst-case exposure remain possible when a budget is exceeded. The
gateway's independent collateral and account limits still apply.

The quote loop renews an account-scoped cancellation lease. Missing heartbeats block placements and amendments
and cause the gateway to cancel resting orders. Deadlines persist across restart;
renewing after expiry cancels the old orders first. The cancellation task checks about every
250 ms. This is not an atomic exchange-engine deadline: already accepted orders may fill
until cancellation commits, and gateway/database downtime delays cancellation. Multi-gateway
active/active routing is unsupported by this in-process ordering guard. The old throughput
benchmarks do not establish performance of the new guarded order path.

`POST /v1/orders/cancel-after` accepts `seconds: 5..300`; `0` cancels and disarms.
`DELETE /v1/orders` cancels only the caller's orders and keeps the lease armed. Both require
the caller's authenticated trading account. The maker intentionally leaves the lease armed
when it exits, as a fallback if graceful cancellation fails.

Input refresh and maximum cache ages are source-specific: Open-Meteo 30 minutes/2 hours,
SWPC 30 minutes/3 hours, DGS10 30 minutes/24 hours, other historical sources 6/24 hours.
Decision logs include input fingerprints, ages, evidence labels and actual quoted sizes.
Age here means time since retrieval, not a guarantee that the provider published new data.

In a second terminal:

```text
uv run --project services --locked python -m brier_services.research monitor --metrics var/marketmaker/metrics.jsonl
```

Open **http://127.0.0.1:3010**. The monitor reads a bounded log tail, refreshes every ten seconds,
shows decisions and input provenance, and has no trading or configuration endpoint. It binds
only to loopback and validates the Host header. An old decision is not proof an order still
rests: the exchange is authoritative. Press **Ctrl+C** in each terminal to stop the maker or
monitor. Saved research reports require no running process.

## Verify changes

`make check` runs Rust formatting/lints/tests and Python formatting/lints/types/tests.
`make test-all` adds PostgreSQL integrations and longer suites. Frontend checks are
`npm run typecheck`, `npm run lint` and `npm run e2e` from `web/`. The report-only checks are
`npm run test:research`; they need uv and Chromium but no database or gateway. Full browser tests need
`BRIER_TEST_DATABASE_URL`, Rust, uv and Playwright Chromium. An installed browser can be used
with `PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH`. See [the implementation record](notes/bot-research-plan.md)
for the specific validation performed for this change.
