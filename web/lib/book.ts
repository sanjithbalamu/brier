"use client";
// The public market-data stream: the `book:<id>` channel (a snapshot, then deltas, each with
// the channel's sequence number) and `trades:<id>`. Same rules as the Python reference client:
// a delta that skips a sequence number means the book is incomplete, so resubscribe and wait
// for a fresh snapshot; never apply a gap.
import { useEffect, useState } from "react";


export type Level = [price: number, qty: number];
export type LiveTrade = { price: number; qty: number; taker_side: "buy" | "sell"; ts: number; seq: number };
export type Book = { synced: boolean; state: string; bids: Map<number, number>; asks: Map<number, number>; gaps: number };

type BookMessage = { type: "snapshot" | "delta"; seq: number; state: string; bids: Level[]; asks: Level[] };

/** Apply one book message; returns the new book, or "resubscribe" on a gap. */
export function applyBook(book: Book & { seq: number | null }, m: BookMessage): (Book & { seq: number | null }) | "resubscribe" | "ignored" {
  if (m.type === "snapshot") {
    if (book.seq !== null && m.seq <= book.seq) return "ignored";
    book = { ...book, bids: new Map(), asks: new Map() };
  } else {
    if (book.seq === null || m.seq <= book.seq) return "ignored";
    if (m.seq !== book.seq + 1) return "resubscribe";
    book = { ...book, bids: new Map(book.bids), asks: new Map(book.asks) };
  }
  for (const [side, levels] of [
    [book.bids, m.bids],
    [book.asks, m.asks],
  ] as const) {
    for (const [price, qty] of levels) {
      if (qty === 0) side.delete(price);
      else side.set(price, qty);
    }
  }
  return { ...book, seq: m.seq, state: m.state, synced: true };
}

type TrackedBook = Book & { seq: number | null; market: number };
function empty(market: number): TrackedBook {
  return { market, synced: false, state: "", bids: new Map(), asks: new Map(), gaps: 0, seq: null };
}

export function useMarketData(market: number, wsUrl: string): { book: Book; trades: LiveTrade[]; connected: boolean } {
  const [book, setBook] = useState<TrackedBook>(() => empty(market));
  const [tape, setTape] = useState<{ market: number; trades: LiveTrade[] }>({ market, trades: [] });
  const [connection, setConnection] = useState<{ market: number; connected: boolean }>({ market, connected: false });

  useEffect(() => {
    let socket: WebSocket | null = null;
    let closed = false;
    let retry = 500;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const channel = `book:${market}`;
    const tradeChannel = `trades:${market}`;
    const connect = () => {
      if (closed) return;
      const ws = new WebSocket(wsUrl);
      socket = ws;
      let current = empty(market);
      let trades: LiveTrade[] = [];
      let tradeSeq: number | null = null;
      const active = () => !closed && socket === ws;
      const send = (op: string, ch: string) => ws.send(JSON.stringify({ op, channel: ch }));
      ws.onopen = () => {
        if (!active()) return;
        retry = 500;
        setConnection({ market, connected: true });
        setBook(current);
        setTape({ market, trades });
        send("subscribe", channel);
        send("subscribe", tradeChannel);
      };
      ws.onmessage = (event) => {
        if (!active()) return;
        let m: { type: string; channel?: string } & Record<string, unknown>;
        try { m = JSON.parse(String(event.data)) as typeof m; }
        catch { ws.close(); return; }
        if (m.channel === channel && (m.type === "snapshot" || m.type === "delta")) {
          const next = applyBook(current, m as unknown as BookMessage);
          if (next === "resubscribe") {
            current = { ...empty(market), gaps: current.gaps + 1 };
            setBook(current);
            send("unsubscribe", channel);
            send("subscribe", channel);
          } else if (next !== "ignored") {
            current = { ...next, market };
            setBook(current);
          }
        } else if (m.channel === tradeChannel && (m.type === "subscribed" || m.type === "resync")) {
          tradeSeq = Number(m.seq);
          trades = [];
          setTape({ market, trades });
        } else if (m.channel === tradeChannel && m.type === "trade") {
          const t = m as unknown as LiveTrade;
          if (tradeSeq !== null && t.seq <= tradeSeq) return;
          if (tradeSeq !== null && t.seq !== tradeSeq + 1) {
            trades = [];
            tradeSeq = null;
            setTape({ market, trades });
            send("unsubscribe", tradeChannel);
            send("subscribe", tradeChannel);
            return;
          }
          tradeSeq = t.seq;
          trades = [t, ...trades].slice(0, 200);
          setTape({ market, trades });
        }
      };
      ws.onclose = () => {
        if (!active()) return;
        setConnection({ market, connected: false });
        current = empty(market);
        setBook(current);
        setTape({ market, trades: [] });
        timer = setTimeout(connect, retry);
        retry = Math.min(retry * 2, 10_000);
      };
    };
    connect();
    return () => {
      closed = true;
      if (timer !== null) clearTimeout(timer);
      socket?.close();
    };
  }, [market, wsUrl]);

  // Render can precede effect cleanup. Never expose another market's snapshot in that window.
  const owned = book.market === market ? book : empty(market);
  return { book: owned, trades: tape.market === market ? tape.trades : [],
    connected: connection.market === market && connection.connected && owned.synced };
}
