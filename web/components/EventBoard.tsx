"use client";
import Link from "next/link";
import { useMemo, useState } from "react";
import { useRouter } from "next/navigation";
import { useMarketData, type Book } from "@/lib/book";
import { planBuy, planSell } from "@/lib/execute";
import { api, ApiError, clientOrderId } from "@/lib/client";
import { compact } from "@/lib/events";
import { credits } from "@/lib/format";
import { OrderBook } from "./OrderBook";
import { TradePanel } from "./TradePanel";
import { MarketSocial } from "./MarketSocial";

export type Outcome = {
  market: number;
  label: string;
  state: string | null;
  chance: number | null;
  change: number | null;
  volume: number;
  bid: number | null;
  ask: number | null;
  rules: string;
  position: number;
};

const REASONS: Record<string, string> = {
  insufficient_collateral: "Not enough available cash for this order.",
  market_not_open: "This market is not open for trading.",
  rate_limited: "Too many orders at once; try again in a moment.",
};

function best(book: Book, fallback: Outcome) {
  if (!book.synced) return { bid: fallback.bid, ask: fallback.ask };
  const bids = [...book.bids.keys()];
  const asks = [...book.asks.keys()];
  return { bid: bids.length ? Math.max(...bids) : null, ask: asks.length ? Math.min(...asks) : null };
}

function Ticket({ o, side, setSide, book, signedIn, question }: { o: Outcome; side: "yes" | "no"; setSide: (s: "yes" | "no") => void; book: Book; signedIn: boolean; question: string }) {
  const router = useRouter();
  const [action, setAction] = useState<"buy" | "sell">("buy");
  const [mode, setMode] = useState<"amount" | "limit">("amount");
  const [amount, setAmount] = useState(10); // credits
  const [contracts, setContracts] = useState(10);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);
  const { bid, ask } = best(book, o);
  const yes = ask;
  const no = bid == null ? null : 100 - bid;
  const open = (book.synced ? book.state : o.state) === "open";
  const held = side === "yes" ? Math.max(0, o.position) : Math.max(0, -o.position);
  const plan = useMemo(() => {
    if (!book.synced) return null;
    return action === "buy" ? planBuy(book, side, Math.round(amount * 100)) : planSell(book, side, Math.min(contracts, held));
  }, [book, action, side, amount, contracts, held]);
  const word = side === "yes" ? "Yes" : "No";

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!plan || !open || busy) return;
    setBusy(true);
    setMessage(null);
    try {
      const r = await api<{ filled: number; status: string }>("POST", "/v1/orders", {
        market: o.market,
        side: plan.side,
        price: plan.price,
        qty: plan.qty,
        tif: "ioc",
        client_order_id: clientOrderId(),
      });
      setMessage({
        ok: r.filled > 0,
        text: r.filled > 0 ? `${action === "buy" ? "Bought" : "Sold"} ${r.filled} ${word} on “${o.label}”.` : "The book moved and nothing filled; try again.",
      });
      router.refresh();
    } catch (err) {
      const code = err instanceof ApiError ? err.code : "";
      setMessage({ ok: false, text: REASONS[code] ?? (err instanceof Error ? err.message : "The order failed.") });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="ticket card">
      <div className="ticket-head">
        <span className="kicker">{question}</span>
        <strong>{o.label}</strong>
      </div>
      <div className="ticket-modes">
        <div className="tabs" role="tablist" aria-label="Buy or sell">
          {(["buy", "sell"] as const).map((a) => (
            <button key={a} type="button" role="tab" aria-selected={action === a} onClick={() => setAction(a)}>
              {a}
            </button>
          ))}
        </div>
        <label className="sr-only" htmlFor="ticket-mode">
          Order type
        </label>
        <select id="ticket-mode" value={mode} onChange={(e) => setMode(e.target.value as "amount" | "limit")}>
          <option value="amount">Market</option>
          <option value="limit">Limit</option>
        </select>
      </div>
      {mode === "limit" ? (
        <TradePanel
          key={`${o.market}-${side}-${action}`}
          market={o.market}
          quotes={{ buyYes: yes, buyNo: no, sellYes: bid, sellNo: ask == null ? null : 100 - ask }}
          signedIn={signedIn}
          canTrade={open}
          position={o.position}
          initialSide={side}
          dataReady={book.synced}
          selection={{ action, side, onChange: (a, s) => { setAction(a); setSide(s); } }}
        />
      ) : (
        <form onSubmit={submit} className="stack" style={{ gap: 14 }}>
          <div className="seg" role="group" aria-label="Outcome">
            <button type="button" className="px yes" aria-pressed={side === "yes"} onClick={() => setSide("yes")}>
              Yes {action === "buy" ? (yes ?? "—") : (bid ?? "—")}
              {(action === "buy" ? yes : bid) != null && "¢"}
            </button>
            <button type="button" className="px no" aria-pressed={side === "no"} onClick={() => setSide("no")}>
              No {action === "buy" ? (no ?? "—") : ask == null ? "—" : 100 - ask}
              {(action === "buy" ? no : ask) != null && "¢"}
            </button>
          </div>
          {action === "buy" ? (
            <>
              <label className="field amount">
                <span>Amount</span>
                <div className="input-adorn">
                  <input type="number" inputMode="decimal" min={1} step={1} value={amount} onChange={(e) => setAmount(Math.max(0, Number(e.target.value)))} />
                  <span className="unit" aria-hidden="true">
                    credits
                  </span>
                </div>
              </label>
              <div className="quick" aria-label="Add credits">
                {[1, 5, 10, 100].map((n) => (
                  <button key={n} type="button" onClick={() => setAmount((a) => a + n)}>
                    +{n}
                  </button>
                ))}
              </div>
            </>
          ) : (
            <label className="field">
              <span>
                Contracts to sell <span className="muted">(you hold {held})</span>
              </span>
              <div className="input-adorn">
                <input type="number" inputMode="numeric" min={1} max={held} value={contracts} onChange={(e) => setContracts(Math.max(0, Math.floor(Number(e.target.value))))} />
                <span className="unit" aria-hidden="true">
                  contracts
                </span>
              </div>
            </label>
          )}
          <div className="summary" aria-live="polite">
            {!book.synced ? (
              <div>
                <span className="muted">Connecting to the book…</span>
              </div>
            ) : plan ? (
              action === "buy" ? (
                <>
                  <div>
                    <span className="muted">Cost</span>
                    <span>{credits(plan.total)}</span>
                  </div>
                  <div>
                    <span className="muted">Average price</span>
                    <span>{plan.average.toFixed(1)}¢</span>
                  </div>
                  <div>
                    <span className="muted">Contracts</span>
                    <span>{plan.qty.toLocaleString("en-US")}</span>
                  </div>
                  <div>
                    <span className="muted">Payout if {word}</span>
                    <span className="payout">
                      {credits(plan.qty * 100)}{" "}
                      <small className="yes">(+{Math.round(((plan.qty * 100) / plan.total - 1) * 100)}%)</small>
                    </span>
                  </div>
                  {plan.total < Math.round(amount * 100) - Math.ceil(plan.average) && (
                    <p className="thin-book">
                      Only {plan.qty.toLocaleString("en-US")} contracts are offered at prices you can reach now, so{" "}
                      {credits(Math.round(amount * 100) - plan.total)} of your amount is not spent. A limit order can wait for more.
                    </p>
                  )}
                </>
              ) : (
                <div>
                  <span className="muted">You receive</span>
                  <span className="payout">{credits(plan.total)}</span>
                </div>
              )
            ) : (
              <div>
                <span className="muted">{action === "sell" && held === 0 ? `You hold no ${word} here.` : "Nothing on the book to fill this now. Try a limit order."}</span>
              </div>
            )}
          </div>
          {!signedIn ? (
            <Link href="/signin" className="btn primary block">
              Log in to trade
            </Link>
          ) : (
            <button className="btn primary block" type="submit" disabled={busy || !open || !plan}>
              {!open ? "Trading is closed" : busy ? "Placing…" : action === "buy" ? `Buy ${word}${plan ? ` · ${credits(plan.total)}` : ""}` : `Sell ${plan?.qty ?? 0} ${word}`}
            </button>
          )}
          {o.position !== 0 && (
            <p className="muted held">
              You hold <strong className={o.position > 0 ? "yes" : "no"}>{Math.abs(o.position)} {o.position > 0 ? "Yes" : "No"}</strong> on this outcome.
            </p>
          )}
          <div aria-live="polite">{message && <p className={`notice ${message.ok ? "ok" : "error"}`}>{message.text}</p>}</div>
        </form>
      )}
    </div>
  );
}

export function EventBoard({
  question,
  outcomes,
  initial,
  initialSide,
  signedIn,
  wsUrl,
}: {
  question: string;
  outcomes: Outcome[];
  initial: number;
  initialSide: "yes" | "no";
  signedIn: boolean;
  wsUrl: string;
}) {
  const [selected, setSelected] = useState(initial);
  const [side, setSide] = useState<"yes" | "no">(initialSide);
  const o = outcomes.find((x) => x.market === selected) ?? outcomes[0]!;
  const { book, connected } = useMarketData(o.market, wsUrl);
  const single = outcomes.length === 1;

  const choose = (market: number, s?: "yes" | "no") => {
    setSelected(market);
    if (s) setSide(s);
    const url = new URL(window.location.href);
    url.searchParams.set("m", String(market));
    if (s) url.searchParams.set("side", s);
    window.history.replaceState(null, "", url);
  };

  return (
    <div className="event-grid">
      <div className="event-main">
        {!single && (
          <div className="otable" role="list" aria-label="Outcomes">
            <div className="ohead" aria-hidden="true">
              <span>Outcome</span>
              <span>Chance</span>
              <span />
            </div>
            {outcomes.map((x) => {
              const live = x.market === o.market ? best(book, x) : { bid: x.bid, ask: x.ask };
              return (
                <div key={x.market} role="listitem" className={`orow${x.market === o.market ? " on" : ""}`}>
                  <button type="button" className="oname" aria-pressed={x.market === o.market} onClick={() => choose(x.market)}>
                    <b>{x.label}</b>
                    <span className="muted">{compact(x.volume)} traded</span>
                  </button>
                  <span className="ochance">
                    <b>{x.chance == null ? "—" : `${x.chance}%`}</b>
                    {x.change ? (
                      <span className={x.change > 0 ? "yes" : "no"}>
                        {x.change > 0 ? "▲" : "▼"} {Math.abs(x.change)}
                      </span>
                    ) : null}
                  </span>
                  <span className="obuy">
                    <button type="button" className="px yes" disabled={x.state !== "open"} onClick={() => choose(x.market, "yes")}>
                      Buy Yes {live.ask == null ? "—" : `${live.ask}¢`}
                    </button>
                    <button type="button" className="px no" disabled={x.state !== "open"} onClick={() => choose(x.market, "no")}>
                      Buy No {live.bid == null ? "—" : `${100 - live.bid}¢`}
                    </button>
                  </span>
                </div>
              );
            })}
          </div>
        )}

        <section className="card pad book-card" aria-labelledby="book-h">
          <div className="spread">
            <h2 id="book-h" className="sr-only">
              Order book{single ? "" : `, ${o.label}`}
            </h2>
            <span className="kicker">{single ? "Yes contracts" : o.label}</span>
            <span className={`live${connected ? " on" : ""}`}>{connected ? "Live" : "Connecting"}</span>
          </div>
          <OrderBook book={book} depth={6} />
        </section>

        <MarketSocial market={o.market} />

        <section className="card pad" aria-labelledby="rules-h">
          <div className="spread">
            <h2 id="rules-h" className="h-sm">
              Rules{single ? "" : ` · ${o.label}`}
            </h2>
            <Link className="link" href={`/markets/${o.market}`}>
              Rulebook, evidence and settlement
            </Link>
          </div>
          <p className="dim rules-text">{o.rules}</p>
        </section>
      </div>
      <aside className="event-side">
        <Ticket key={o.market} o={o} side={side} setSide={setSide} book={book} signedIn={signedIn} question={question} />
      </aside>
    </div>
  );
}
