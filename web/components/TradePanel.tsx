"use client";
import Link from "next/link";
import { useState } from "react";
import { useRouter } from "next/navigation";
import { api, ApiError, clientOrderId } from "@/lib/client";
import { credits } from "@/lib/format";

type Result = { status: string; filled: number; remaining: number; cancel_reason: string | null; order: number };
type Quotes = { buyYes: number | null; buyNo: number | null; sellYes: number | null; sellNo: number | null };

const REASONS: Record<string, string> = {
  insufficient_collateral: "Not enough available cash for this order.",
  market_not_open: "The market is not open for trading.",
  invalid_price: "Prices are 1¢ to 99¢.",
  invalid_quantity: "Contracts are 1 to 25,000 per order.",
  fok_unfillable: "Not enough on the book to fill all of it now.",
  too_many_orders: "You have the most resting orders allowed.",
  rate_limited: "Too many orders at once; try again in a moment.",
};

/** Buy or sell YES or NO. In the single YES book (SPEC §2.2): buy YES at p is a bid at p; buy NO
 * at q is an offer of YES at 100 − q; selling is the reverse. */
export function TradePanel({
  market,
  quotes,
  signedIn,
  canTrade,
  position,
  initialSide,
  selection,
  dataReady = true,
}: {
  market: number;
  quotes: Quotes;
  signedIn: boolean;
  canTrade: boolean;
  position: number;
  initialSide: "yes" | "no";
  selection?: { action: "buy" | "sell"; side: "yes" | "no"; onChange: (action: "buy" | "sell", side: "yes" | "no") => void };
  dataReady?: boolean;
}) {
  const router = useRouter();
  const [localAction, setAction] = useState<"buy" | "sell">("buy");
  const [localSide, setSide] = useState<"yes" | "no">(initialSide);
  const action = selection?.action ?? localAction;
  const side = selection?.side ?? localSide;
  const quote = (a: "buy" | "sell", s: "yes" | "no") => quotes[`${a}${s === "yes" ? "Yes" : "No"}` as keyof Quotes];
  const [price, setPrice] = useState<number>(quote(action, side) ?? 50);
  const [qty, setQty] = useState(10);
  const [tif, setTif] = useState<"gtc" | "ioc">("gtc");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);

  const choose = (a: "buy" | "sell", s: "yes" | "no") => {
    setAction(a);
    setSide(s);
    selection?.onChange(a, s);
    setPrice(quote(a, s) ?? 50);
    setMessage(null);
  };
  const held = side === "yes" ? Math.max(0, position) : Math.max(0, -position);
  const valid = Number.isInteger(price) && price >= 1 && price <= 99 && Number.isInteger(qty) && qty >= 1 && qty <= 25_000 && (action === "buy" || qty <= held);
  const word = side === "yes" ? "Yes" : "No";

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (!valid || !canTrade || !dataReady || busy) return;
    setBusy(true);
    setMessage(null);
    const bookSide = (action === "buy") === (side === "yes") ? "buy" : "sell";
    const bookPrice = side === "yes" ? price : 100 - price;
    try {
      const r = await api<Result>("POST", "/v1/orders", { market, side: bookSide, price: bookPrice, qty, tif, client_order_id: clientOrderId() });
      const filled = `${r.filled} of ${qty} filled`;
      setMessage({
        ok: true,
        text:
          r.status === "resting"
            ? `${filled}; ${r.remaining} resting at ${price}¢ (order ${r.order}).`
            : r.status === "cancelled"
              ? `${filled}; the rest was cancelled.`
              : `${filled}.`,
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
    <form className="card panel" onSubmit={submit} aria-label="Trade">
      {!selection && <div className="tabs" role="tablist" aria-label="Buy or sell">
        {(["buy", "sell"] as const).map((a) => (
          <button key={a} type="button" role="tab" aria-selected={action === a} onClick={() => choose(a, side)}>
            {a}
          </button>
        ))}
      </div>}
      <div className="seg" role="group" aria-label="Outcome">
        <button type="button" className="px yes" aria-pressed={side === "yes"} onClick={() => choose(action, "yes")}>
          Yes {quote(action, "yes") ?? "—"}
          {quote(action, "yes") != null && "¢"}
        </button>
        <button type="button" className="px no" aria-pressed={side === "no"} onClick={() => choose(action, "no")}>
          No {quote(action, "no") ?? "—"}
          {quote(action, "no") != null && "¢"}
        </button>
      </div>
      <label className="field">
        <span>Contracts{action === "sell" && <span className="muted"> (you hold {held})</span>}</span>
        <div className="input-adorn">
          <input type="number" inputMode="numeric" min={1} max={action === "sell" ? held : 25000} value={qty} onChange={(e) => setQty(Number(e.target.value))} />
          <span className="unit" aria-hidden="true">contracts</span>
        </div>
      </label>
      <div className="quick" aria-label="Add contracts">
        {[1, 10, 100, 1000].map((n) => (
          <button key={n} type="button" onClick={() => setQty((q) => Math.min(25_000, q + n))}>
            +{n.toLocaleString("en-US")}
          </button>
        ))}
      </div>
      <label className="field">
        <span>Limit price for {word}</span>
        <div className="input-adorn">
          <input type="number" inputMode="numeric" min={1} max={99} value={price} onChange={(e) => setPrice(Number(e.target.value))} />
          <span className="unit" aria-hidden="true">¢</span>
        </div>
      </label>
      <label className="field">
        <span>If it does not fill at once</span>
        <select value={tif} onChange={(e) => setTif(e.target.value as "gtc" | "ioc")}>
          <option value="gtc">Keep the rest in the book</option>
          <option value="ioc">Cancel the rest</option>
        </select>
      </label>
      {valid && (
        <div className="summary">
          {action === "buy" ? (
            <>
              <div>
                <span className="muted">Cost</span>
                <span>{credits(qty * price)}</span>
              </div>
              <div>
                <span className="muted">Payout if {word}</span>
                <span className="payout">{credits(qty * 100)}</span>
              </div>
            </>
          ) : (
            <div>
              <span className="muted">You receive</span>
              <span className="payout">{credits(qty * price)}</span>
            </div>
          )}
        </div>
      )}
      {!signedIn ? (
        <Link href="/signin" className="btn primary block">
          Log in to trade
        </Link>
      ) : (
        <button className="btn primary block" type="submit" disabled={busy || !canTrade || !dataReady || !valid}>
          {!canTrade ? "Trading is closed" : !dataReady ? "Waiting for live prices…" : busy ? "Placing…" : `${action === "buy" ? "Buy" : "Sell"} ${qty.toLocaleString("en-US")} ${word} at ${price}¢`}
        </button>
      )}
      {position !== 0 && (
        <p className="muted" style={{ margin: 0, fontSize: 14 }}>
          You hold <strong className={position > 0 ? "yes" : "no"}>{Math.abs(position)} {position > 0 ? "Yes" : "No"}</strong>.
        </p>
      )}
      <div aria-live="polite">{message && <p className={`notice ${message.ok ? "ok" : "error"}`}>{message.text}</p>}</div>
    </form>
  );
}
