//! PostgreSQL read models, projected from the journal (docs/gates/G4-plan.md).
//!
//! The [`Projector`] follows the journal up to the core's durable sequence number and replays
//! each command through its own copy of the engine. The engine is deterministic, so the copy's
//! events equal the core's, and the read models are a function of the journal alone: they can
//! be rebuilt at any time, and a test checks that a rebuild equals the live tables. Each batch
//! of rows is written in one transaction together with the projector's cursor, so a crash
//! between batches can neither lose nor repeat a row.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use brier_engine::{CancelReason, Command, Engine, Event, RejectReason, Sequenced};
use brier_journal::{Follower, JournalError};
use brier_types::{AccountId, Digest, MarketId, OrderId, Outcome, Seq, Side};
use tokio_postgres::Client;

/// Embedded migrations for the whole database (read models and gateway identity), applied in
/// order, each once (`schema_migrations`).
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_read_models",
        include_str!("../migrations/0001_read_models.sql"),
    ),
    (
        "0002_identity",
        include_str!("../migrations/0002_identity.sql"),
    ),
    (
        "0003_market_trades",
        include_str!("../migrations/0003_market_trades.sql"),
    ),
    (
        "0004_resolution",
        include_str!("../migrations/0004_resolution.sql"),
    ),
    (
        "0005_forecasts",
        include_str!("../migrations/0005_forecasts.sql"),
    ),
    ("0006_totp", include_str!("../migrations/0006_totp.sql")),
    (
        "0007_surveillance",
        include_str!("../migrations/0007_surveillance.sql"),
    ),
    ("0008_events", include_str!("../migrations/0008_events.sql")),
    (
        "0009_automatic_review",
        include_str!("../migrations/0009_automatic_review.sql"),
    ),
    ("0010_lister", include_str!("../migrations/0010_lister.sql")),
    (
        "0011_public_accounts",
        include_str!("../migrations/0011_public_accounts.sql"),
    ),
    (
        "0012_allowance",
        include_str!("../migrations/0012_allowance.sql"),
    ),
    ("0013_social", include_str!("../migrations/0013_social.sql")),
    (
        "0014_order_leases",
        include_str!("../migrations/0014_order_leases.sql"),
    ),
];

/// Arbitrary but fixed key for the advisory lock that serializes migrations across processes.
const MIGRATION_LOCK: i64 = 0x6272_6965_725f_6d67; // "brier_mg"

/// A read-model failure.
#[derive(Debug)]
pub enum ReadModelError {
    /// The database failed.
    Db(tokio_postgres::Error),
    /// The journal failed or is corrupt.
    Journal(JournalError),
    /// Replaying the journal halted the engine copy, which means the journal and the engine
    /// disagree; projection stops.
    Engine(String),
    /// The cursor moved underneath this projector: another projector is writing.
    CursorConflict,
    /// A value does not fit its column.
    OutOfRange(&'static str),
}

impl fmt::Display for ReadModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {}", describe_db_error(e)),
            Self::Journal(e) => write!(f, "{e}"),
            Self::Engine(e) => write!(f, "replay: {e}"),
            Self::CursorConflict => {
                f.write_str("the projector cursor changed underneath this projector")
            }
            Self::OutOfRange(what) => write!(f, "{what} does not fit its column"),
        }
    }
}

/// A database error with PostgreSQL's own code and message (the plain `Display` of a server
/// error is only "db error").
#[must_use]
pub fn describe_db_error(e: &tokio_postgres::Error) -> String {
    e.as_db_error().map_or_else(
        || e.to_string(),
        |d| format!("{} ({})", d.message(), d.code().code()),
    )
}

impl std::error::Error for ReadModelError {}

impl From<tokio_postgres::Error> for ReadModelError {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Db(e)
    }
}

impl From<JournalError> for ReadModelError {
    fn from(e: JournalError) -> Self {
        Self::Journal(e)
    }
}

fn col(v: u64, what: &'static str) -> Result<i64, ReadModelError> {
    i64::try_from(v).map_err(|_| ReadModelError::OutOfRange(what))
}

/// Apply every migration not yet applied, each in its own transaction, under an advisory lock.
///
/// # Errors
/// [`ReadModelError::Db`] if a statement fails; that migration is rolled back.
pub async fn migrate(client: &mut Client) -> Result<(), ReadModelError> {
    client
        .batch_execute("CREATE TABLE IF NOT EXISTS schema_migrations (name text PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now())")
        .await?;
    for (name, sql) in MIGRATIONS {
        let tx = client.transaction().await?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
            .await?;
        if tx
            .query_opt("SELECT 1 FROM schema_migrations WHERE name = $1", &[name])
            .await?
            .is_none()
        {
            tx.batch_execute(sql).await?;
            tx.execute("INSERT INTO schema_migrations (name) VALUES ($1)", &[name])
                .await?;
        }
        tx.commit().await?;
    }
    Ok(())
}

struct Fill {
    seq: i64,
    ordinal: i32,
    market: i64,
    maker: i64,
    taker: i64,
    maker_account: i64,
    taker_account: i64,
    taker_side: &'static str,
    price: i16,
    qty: i32,
}

struct OrderEvent {
    seq: i64,
    ordinal: i32,
    order_id: Option<i64>,
    account: i64,
    market: i64,
    kind: &'static str,
    remaining: Option<i32>,
    reason: Option<&'static str>,
}

/// What the journal says about a market's rules or settlement (`market_journal`).
enum Pin {
    Listed {
        market: i64,
        seq: i64,
        rules_hash: Option<Digest>,
    },
    Settled {
        market: i64,
        seq: i64,
        outcome: &'static str,
        evidence_hash: Option<Digest>,
    },
}

/// The rows one batch of commands produces.
#[derive(Default)]
struct Rows {
    fills: Vec<Fill>,
    order_events: Vec<OrderEvent>,
    pins: Vec<Pin>,
}

/// The `market_journal` change an event makes, if any.
fn pin(event: &Event, seq: i64) -> Option<Pin> {
    match *event {
        Event::MarketCreated { market, rules_hash } => Some(Pin::Listed {
            market: i64::from(market.get()),
            seq,
            rules_hash,
        }),
        Event::Settled {
            market,
            outcome,
            evidence_hash,
        } => Some(Pin::Settled {
            market: i64::from(market.get()),
            seq,
            outcome: outcome_name(outcome),
            evidence_hash,
        }),
        _ => None,
    }
}

fn digest_col(d: Option<Digest>) -> Option<Vec<u8>> {
    d.map(|d| d.bytes().to_vec())
}

const fn outcome_name(o: Outcome) -> &'static str {
    match o {
        Outcome::Yes => "yes",
        Outcome::No => "no",
        Outcome::Void => "void",
    }
}

const fn cancel_reason(r: CancelReason) -> &'static str {
    match r {
        CancelReason::User => "user",
        CancelReason::IocRemainder => "ioc_remainder",
        CancelReason::SelfTrade => "self_trade",
        CancelReason::MarketClosed => "market_closed",
        CancelReason::KillSwitch => "kill_switch",
    }
}

const fn reject_reason(r: RejectReason) -> &'static str {
    match r {
        RejectReason::UnknownMarket => "unknown_market",
        RejectReason::MarketExists => "market_exists",
        RejectReason::MarketNotOpen => "market_not_open",
        RejectReason::InvalidTransition => "invalid_transition",
        RejectReason::InvalidOrder => "invalid_order",
        RejectReason::InvalidPrice => "invalid_price",
        RejectReason::InvalidQuantity => "invalid_quantity",
        RejectReason::PostOnlyWouldCross => "post_only_would_cross",
        RejectReason::FokUnfillable => "fok_unfillable",
        RejectReason::UnknownOrder => "unknown_order",
        RejectReason::InvalidAmend => "invalid_amend",
        RejectReason::UnknownAccount => "unknown_account",
        RejectReason::AccountExists => "account_exists",
        RejectReason::TooManyOrders => "too_many_orders",
        RejectReason::InsufficientCollateral => "insufficient_collateral",
    }
}

/// Follows the journal and keeps the read models current.
pub struct Projector {
    follower: Follower,
    replica: Engine,
    /// Owner and market of every order still live in the replica.
    owners: HashMap<OrderId, (AccountId, MarketId)>,
    cursor: u64,
}

impl Projector {
    /// Open the projector for the journal in `dir`: read the cursor, then replay the journal up
    /// to it through the engine copy (writing nothing), so projection resumes exactly there.
    ///
    /// # Errors
    /// A [`ReadModelError`] if the database, the journal, or the replay fails.
    pub async fn open(client: &Client, dir: &Path) -> Result<Self, ReadModelError> {
        let row = client
            .query_one("SELECT last_seq FROM projector_cursor WHERE id = 1", &[])
            .await?;
        let cursor = u64::try_from(row.get::<_, i64>(0))
            .map_err(|_| ReadModelError::OutOfRange("cursor"))?;
        let mut p = Self {
            follower: Follower::new(dir, Seq::FIRST),
            replica: Engine::new(),
            owners: HashMap::new(),
            cursor,
        };
        let mut ignored = Rows::default();
        let mut commands = Vec::new();
        p.follower.poll::<ReadModelError>(cursor, |seq, command| {
            commands.push((seq, command));
            Ok(())
        })?;
        for (seq, command) in commands {
            p.replay(seq, command, &mut ignored)?;
        }
        Ok(p)
    }

    /// The last sequence number reflected in the read models.
    #[must_use]
    pub const fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Project up to `max` new commands, never beyond `durable`, in one transaction. Returns
    /// how many were projected.
    ///
    /// # Errors
    /// A [`ReadModelError`]; the transaction is then rolled back and the cursor is unchanged.
    pub async fn project(
        &mut self,
        client: &mut Client,
        durable: u64,
        max: u64,
    ) -> Result<u64, ReadModelError> {
        let bound = durable.min(self.cursor.saturating_add(max));
        let mut commands = Vec::new();
        self.follower
            .poll::<ReadModelError>(bound, |seq, command| {
                commands.push((seq, command));
                Ok(())
            })?;
        if commands.is_empty() {
            return Ok(0);
        }
        let mut rows = Rows::default();
        for &(seq, command) in &commands {
            self.replay(seq, command, &mut rows)?;
        }
        let Rows {
            fills,
            order_events,
            pins,
        } = rows;
        let new_cursor = commands.last().map_or(self.cursor, |(s, _)| s.get());
        let tx = client.transaction().await?;
        let insert_fill = tx
            .prepare("INSERT INTO fills (seq, ordinal, market, maker, taker, maker_account, taker_account, taker_side, price, qty) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
            .await?;
        for f in &fills {
            tx.execute(
                &insert_fill,
                &[
                    &f.seq,
                    &f.ordinal,
                    &f.market,
                    &f.maker,
                    &f.taker,
                    &f.maker_account,
                    &f.taker_account,
                    &f.taker_side,
                    &f.price,
                    &f.qty,
                ],
            )
            .await?;
        }
        // When these fills were first projected (`fill_times`; the journal has no clock).
        if let (Some(first), Some(last)) = (fills.first(), fills.last()) {
            tx.execute(
                "INSERT INTO fill_times (seq) SELECT DISTINCT seq FROM fills WHERE seq BETWEEN $1 AND $2 ON CONFLICT DO NOTHING",
                &[&first.seq, &last.seq],
            )
            .await?;
        }
        let insert_event = tx
            .prepare("INSERT INTO order_events (seq, ordinal, order_id, account, market, kind, remaining, reason) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
            .await?;
        for e in &order_events {
            tx.execute(
                &insert_event,
                &[
                    &e.seq,
                    &e.ordinal,
                    &e.order_id,
                    &e.account,
                    &e.market,
                    &e.kind,
                    &e.remaining,
                    &e.reason,
                ],
            )
            .await?;
        }
        write_pins(&tx, &pins).await?;
        let moved = tx
            .execute(
                "UPDATE projector_cursor SET last_seq = $1 WHERE id = 1 AND last_seq = $2",
                &[&col(new_cursor, "cursor")?, &col(self.cursor, "cursor")?],
            )
            .await?;
        if moved != 1 {
            return Err(ReadModelError::CursorConflict);
        }
        tx.commit().await?;
        self.cursor = new_cursor;
        let projected = commands.len() as u64;
        Ok(projected)
    }

    /// Apply one command to the engine copy and turn its events into rows.
    fn replay(
        &mut self,
        seq: Seq,
        command: Command,
        rows: &mut Rows,
    ) -> Result<(), ReadModelError> {
        let mut events = Vec::new();
        self.replica
            .apply(&Sequenced { seq, command }, &mut events)
            .map_err(|e| ReadModelError::Engine(e.to_string()))?;
        let s = col(seq.get(), "seq")?;
        let placed = match command {
            Command::Place(o) => Some((o.account, o.market)),
            _ => None,
        };
        let missing = |what: &str| ReadModelError::Engine(format!("{what} at seq {}", seq.get()));
        for (i, event) in events.iter().enumerate() {
            if let Some(pin) = pin(event, s) {
                rows.pins.push(pin);
                continue;
            }
            let at = (
                s,
                i32::try_from(i).map_err(|_| ReadModelError::OutOfRange("ordinal"))?,
            );
            let row = match *event {
                Event::Accepted { order } => {
                    let owner = placed.ok_or_else(|| missing("accepted without a place"))?;
                    self.owners.insert(order, owner);
                    order_row(at, Some(order), owner, "accepted", None, None)?
                }
                Event::Rested { order, remaining } | Event::Amended { order, remaining } => {
                    let owner = *self
                        .owners
                        .get(&order)
                        .ok_or_else(|| missing("order has no owner"))?;
                    let kind = if matches!(event, Event::Rested { .. }) {
                        "rested"
                    } else {
                        "amended"
                    };
                    order_row(
                        at,
                        Some(order),
                        owner,
                        kind,
                        Some(remaining.contracts()),
                        None,
                    )?
                }
                Event::Cancelled {
                    order,
                    remaining,
                    reason,
                } => {
                    let owner = self
                        .owners
                        .remove(&order)
                        .ok_or_else(|| missing("cancelled order has no owner"))?;
                    order_row(
                        at,
                        Some(order),
                        owner,
                        "cancelled",
                        Some(remaining.contracts()),
                        Some(cancel_reason(reason)),
                    )?
                }
                Event::Duplicate { original } => order_row(
                    at,
                    Some(original),
                    placed.ok_or_else(|| missing("duplicate without a place"))?,
                    "duplicate",
                    None,
                    None,
                )?,
                Event::Rejected { reason } => match placed {
                    Some(owner) => order_row(
                        at,
                        None,
                        owner,
                        "rejected",
                        None,
                        Some(reject_reason(reason)),
                    )?,
                    None => continue,
                },
                Event::Trade(t) => {
                    rows.fills.push(fill_row(at, &t)?);
                    continue;
                }
                _ => continue,
            };
            rows.order_events.push(row);
        }
        // Orders that filled completely leave the book without a cancel event, and incoming
        // orders that never rested leave none either: forget every owner no longer live.
        for e in &events {
            let order = match *e {
                Event::Accepted { order } => order,
                Event::Trade(t) => t.maker,
                _ => continue,
            };
            if !self.replica.order_is_live(order) {
                self.owners.remove(&order);
            }
        }
        Ok(())
    }
}

/// Write `market_journal` rows in journal order: a market listed and settled in one batch is
/// inserted, then updated.
async fn write_pins(
    tx: &tokio_postgres::Transaction<'_>,
    pins: &[Pin],
) -> Result<(), ReadModelError> {
    for pin in pins {
        let changed = match pin {
            Pin::Listed {
                market,
                seq,
                rules_hash,
            } => {
                tx.execute(
                    "INSERT INTO market_journal (market, listed_seq, rules_hash) VALUES ($1, $2, $3)",
                    &[market, seq, &digest_col(*rules_hash)],
                )
                .await?
            }
            Pin::Settled {
                market,
                seq,
                outcome,
                evidence_hash,
            } => {
                tx.execute(
                    "UPDATE market_journal SET settled_seq = $2, outcome = $3, evidence_hash = $4 WHERE market = $1 AND settled_seq IS NULL",
                    &[market, seq, outcome, &digest_col(*evidence_hash)],
                )
                .await?
            }
        };
        if changed != 1 {
            return Err(ReadModelError::Engine(
                "market_journal disagrees with the journal".to_owned(),
            ));
        }
    }
    Ok(())
}

fn order_row(
    (seq, ordinal): (i64, i32),
    order: Option<OrderId>,
    (account, market): (AccountId, MarketId),
    kind: &'static str,
    remaining: Option<u32>,
    reason: Option<&'static str>,
) -> Result<OrderEvent, ReadModelError> {
    Ok(OrderEvent {
        seq,
        ordinal,
        order_id: order.map(|o| col(o.get(), "order")).transpose()?,
        account: col(account.get(), "account")?,
        market: i64::from(market.get()),
        kind,
        remaining: remaining
            .map(|r| i32::try_from(r).map_err(|_| ReadModelError::OutOfRange("remaining")))
            .transpose()?,
        reason,
    })
}

fn fill_row((seq, ordinal): (i64, i32), t: &brier_engine::Trade) -> Result<Fill, ReadModelError> {
    Ok(Fill {
        seq,
        ordinal,
        market: i64::from(t.market.get()),
        maker: col(t.maker.get(), "maker")?,
        taker: col(t.taker.get(), "taker")?,
        maker_account: col(t.maker_account.get(), "maker account")?,
        taker_account: col(t.taker_account.get(), "taker account")?,
        taker_side: match t.taker_side {
            Side::Buy => "buy",
            Side::Sell => "sell",
        },
        price: i16::from(t.price.ticks()),
        qty: i32::try_from(t.qty.contracts()).map_err(|_| ReadModelError::OutOfRange("qty"))?,
    })
}
