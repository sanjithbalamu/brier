"use client";
import { useMarketData } from "@/lib/book";
import { PriceChart } from "./PriceChart";
import { OrderBook } from "./OrderBook";
import { TradePanel } from "./TradePanel";

type Trade = { seq: number; price: number; qty: number; taker_side: string };

/** Everything on the market page that moves: one WebSocket feeds the chart, the book, the trade
 * list, and the trade panel's prices. Server-rendered sections arrive as `below` and `rail`. */
export function MarketLive({
  market,
  points,
  recent,
  wsUrl,
  signedIn,
  canTrade,
  position,
  initialSide,
  initialQuotes,
  header,
  below,
  rail,
}: {
  market: number;
  points: [string, number][];
  recent: Trade[];
  wsUrl: string;
  signedIn: boolean;
  canTrade: boolean;
  position: number;
  initialSide: "yes" | "no";
  /** Prices from the server's render, shown until the live book has synced. */
  initialQuotes: { buyYes: number | null; buyNo: number | null; sellYes: number | null; sellNo: number | null };
  header: React.ReactNode;
  below: React.ReactNode;
  rail: React.ReactNode;
}) {
  const { book, trades, connected } = useMarketData(market, wsUrl);
  const live: [string, number][] = trades
    .slice()
    .reverse()
    .map((t) => [new Date(Math.floor(t.ts / 1000)).toISOString(), t.price]);
  const bestBid = book.synced ? Math.max(...book.bids.keys(), -Infinity) : -Infinity;
  const bestAsk = book.synced ? Math.min(...book.asks.keys(), Infinity) : Infinity;
  const quotes = book.synced
    ? {
        buyYes: Number.isFinite(bestAsk) ? bestAsk : null,
        buyNo: Number.isFinite(bestBid) ? 100 - bestBid : null,
        sellYes: Number.isFinite(bestBid) ? bestBid : null,
        sellNo: Number.isFinite(bestAsk) ? 100 - bestAsk : null,
      }
    : initialQuotes;
  const tape = [
    ...trades.map((t) => ({ key: `l${t.seq}`, ...t })),
    ...recent.map((t) => ({ key: `r${t.seq}-${t.price}-${t.qty}`, ...t })),
  ].slice(0, 7);

  return (
    <div className="with-rail">
      <div className="stack" style={{ gap: 22 }}>
        {header}
        <section className="card pad" aria-label="Price history">
          <PriceChart points={[...points, ...live]} />
        </section>
        <div className="grid-2" style={{ gridTemplateColumns: "repeat(auto-fit, minmax(min(100%, 320px), 1fr))" }}>
          <section className="card">
            <OrderBook book={book} />
          </section>
          <section className="card">
            <div className="spread" style={{ padding: "16px 16px 8px" }}>
              <h2 style={{ fontSize: 16 }}>Recent trades</h2>
              <span className="muted" style={{ fontSize: 13 }} role="status">
                {connected ? "● Live" : "Reconnecting…"}
              </span>
            </div>
            <table>
              <thead>
                <tr>
                  <th scope="col">Taker bought</th>
                  <th scope="col" className="num">Yes price</th>
                  <th scope="col" className="num">Contracts</th>
                </tr>
              </thead>
              <tbody>
                {tape.map((t) => (
                  <tr key={t.key}>
                    <td className={t.taker_side === "buy" ? "yes" : "no"}>{t.taker_side === "buy" ? "Yes" : "No"}</td>
                    <td className="num">{t.price}¢</td>
                    <td className="num">{t.qty.toLocaleString("en-US")}</td>
                  </tr>
                ))}
                {tape.length === 0 && (
                  <tr>
                    <td colSpan={3} className="muted">
                      No trades yet.
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </section>
        </div>
        {below}
      </div>
      <aside className="rail stack" aria-label="Trade and forecast">
        <TradePanel market={market} quotes={quotes} signedIn={signedIn} canTrade={book.synced ? book.state === "open" : canTrade} dataReady={book.synced} position={position} initialSide={initialSide} />
        {rail}
      </aside>
    </div>
  );
}
