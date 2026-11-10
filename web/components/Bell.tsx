"use client";
import Link from "next/link";
import { useEffect, useRef, useState } from "react";
import { api } from "@/lib/client";
import { credits, relative } from "@/lib/format";
import { marketHref, tradeWords, type Notification, type Notifications } from "@/lib/social";

/** Check for new notifications this often; fills arrive within a second of the trade. */
const POLL_MS = 30_000;

function Line({ n, now }: { n: Notification; now: number }) {
  const title = n.title ?? (n.market == null ? "" : `Market ${n.market}`);
  let text: React.ReactNode;
  if (n.kind === "fill" && n.side && n.price != null && n.qty != null) {
    const w = tradeWords(n.side, n.price);
    text = (
      <>
        Filled: <b className={w.word === "Yes" ? "yes" : "no"}>{n.qty.toLocaleString("en-US")} {w.word}</b> at {w.price}¢
      </>
    );
  } else if (n.kind === "settled") {
    const held = n.qty ?? 0;
    const won = (held > 0 && n.result === "yes") || (held < 0 && n.result === "no");
    text =
      n.result === "void" ? (
        <>Voided: each held contract paid 0.50 credit</>
      ) : held === 0 ? (
        <>Settled {n.result === "yes" ? "Yes" : "No"}</>
      ) : (
        <>
          Settled {n.result === "yes" ? "Yes" : "No"}: your {Math.abs(held).toLocaleString("en-US")} {held > 0 ? "Yes" : "No"}{" "}
          {won ? <b className="yes">paid {credits(Math.abs(held) * 100)}</b> : <b className="no">expired worthless</b>}
        </>
      );
  } else {
    text = <>Daily credits added: <b className="yes">+100</b></>;
  }
  const body = (
    <>
      {n.unread && <span className="unread-dot" aria-label="Unread" />}
      <span className="what">
        <span className="t">{text}</span>
        {title && (
          <span className="muted">
            {title}
            {n.outcome ? ` · ${n.outcome}` : ""}
          </span>
        )}
      </span>
      {n.at && <span className="when">{relative(n.at, now)}</span>}
    </>
  );
  return n.kind === "allowance" ? <div className="note">{body}</div> : <Link className="note" href={marketHref(n)}>{body}</Link>;
}

export function Bell() {
  const [data, setData] = useState<Notifications | null>(null);
  const [open, setOpen] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const next = await api<Notifications>("GET", "/v1/notifications");
        if (alive) {
          setData(next);
          setNow(Date.now());
        }
      } catch {
        // Signed out or unreachable: keep the bell quiet.
      }
    };
    void load();
    const timer = setInterval(load, POLL_MS);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, []);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent | KeyboardEvent) => {
      if (e instanceof KeyboardEvent ? e.key === "Escape" : !box.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", close);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", close);
    };
  }, [open]);

  async function toggle() {
    const next = !open;
    setOpen(next);
    const newest = data?.items[0]?.seq;
    if (next && newest != null && (data?.unread ?? 0) > 0) {
      try {
        await api("POST", "/v1/notifications/seen", { seq: newest });
        // The badge clears; the dots stay while the panel is open, to show what was new.
        setData((d) => (d ? { ...d, unread: 0 } : d));
      } catch {
        // Marked read on the next try.
      }
    }
  }

  const unread = data?.unread ?? 0;
  return (
    <div className="bell" ref={box}>
      <button type="button" className="icon-btn" aria-label={unread ? `Notifications, ${unread} unread` : "Notifications"} aria-expanded={open} onClick={toggle}>
        <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true">
          <path d="M6 9a6 6 0 1 1 12 0c0 6 2.5 7.5 2.5 7.5h-17S6 15 6 9Z" strokeLinejoin="round" />
          <path d="M10 20a2.2 2.2 0 0 0 4 0" strokeLinecap="round" />
        </svg>
        {unread > 0 && <span className="badge">{unread > 9 ? "9+" : unread}</span>}
      </button>
      {open && (
        <div className="bell-panel card" role="dialog" aria-label="Notifications">
          <div className="spread" style={{ padding: "14px 16px 8px" }}>
            <h2 className="h-sm" style={{ margin: 0 }}>Notifications</h2>
            <Link href="/portfolio" className="link" onClick={() => setOpen(false)}>
              Portfolio
            </Link>
          </div>
          {!data || data.items.length === 0 ? (
            <p className="empty">Fills, settlements and daily credits show up here.</p>
          ) : (
            <div className="notes">
              {data.items.map((n) => (
                <Line key={`${n.kind}-${n.seq}-${n.market ?? 0}`} n={n} now={now} />
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
