// Exercise the generated offline artifacts in a real browser, including keyboard controls.
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { expect, test } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

const ROOT = resolve(__dirname, "../..");
function research(args: string[]) {
  return execFileSync("uv", ["run", "--project", "services", "--locked", "python", "-m", "brier_services.research", ...args], { cwd: ROOT, encoding: "utf8" });
}

test("an offline experiment report replays decisions and never requests a remote resource", async ({ page }) => {
  const work = mkdtempSync(join(tmpdir(), "brier-report-"));
  try {
    research(["run", "--out", work, "--seeds", "1", "--scenarios", "disconnect"]);
    const report = readdirSync(work).find((name) => name.endsWith(".html"))!;
    const external: string[] = [];
    const document = "http://research.test/report";
    await page.route(document, (route) => route.fulfill({ body: readFileSync(join(work, report)), contentType: "text/html" }));
    page.on("request", (request) => { if (request.url() !== document) external.push(request.url()); });
    await page.goto(document);
    await expect(page.getByRole("heading", { name: "Understand the strategy." })).toBeVisible();
    await expect(page.locator("#results tr")).toHaveCount(2);
    await page.getByRole("slider", { name: "Step", exact: true }).focus();
    await page.keyboard.press("End");
    await expect(page.locator("#settlement")).toContainText("Final settlement");
    await page.getByRole("button", { name: "Previous step" }).click();
    await expect(page.locator("#settlement")).toContainText("revealed at the end");
    await page.getByLabel("Experiment").selectOption("1");
    await expect(page.locator("#step-label")).toHaveText("0 / 479");
    await expect(page.locator("#trades tr")).toHaveCount(0);
    await page.getByRole("button", { name: "Play", exact: true }).click();
    await expect(page.getByRole("button", { name: "Pause", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Pause", exact: true }).click();
    const a11y = await new AxeBuilder({ page }).withTags(["wcag2a", "wcag2aa", "wcag21aa", "wcag22aa"]).analyze();
    expect(a11y.violations.map((v) => v.id)).toEqual([]);
    expect(external).toEqual([]);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});

test("an observed recording hides the book across a sequence gap", async ({ page }) => {
  const work = mkdtempSync(join(tmpdir(), "brier-history-"));
  try {
    const base = { channel: "book:101", journal_seq: 1, state: "open" };
    const rows = [
      { schema: 1, kind: "observed public market data", market: 101, source: "local test fixture", started_at: "2026-11-10T00:00:00Z" },
      { elapsed: 0, message: { ...base, type: "snapshot", seq: 1, bids: [[40, 10]], asks: [[60, 20]] } },
      { elapsed: 1, message: { ...base, type: "delta", seq: 3, bids: [[50, 10]], asks: [] } },
      { elapsed: 2, message: { ...base, type: "snapshot", seq: 3, bids: [[50, 10]], asks: [[70, 20]] } },
    ];
    const recording = join(work, "recording.jsonl"), output = join(work, "history.html");
    writeFileSync(recording, rows.map((r) => JSON.stringify(r)).join("\n"));
    research(["history-report", recording, "--out", output]);
    await page.route("http://research.test/history", (route) => route.fulfill({ body: readFileSync(output), contentType: "text/html" }));
    await page.goto("http://research.test/history");
    await expect(page.locator("#bids")).toContainText("40¢");
    await page.getByRole("button", { name: "Next message" }).click();
    await expect(page.locator("#status")).toContainText("Book unavailable");
    await expect(page.locator("#bids tr")).toHaveCount(0);
    await page.getByRole("button", { name: "Next message" }).click();
    await expect(page.locator("#bids")).toContainText("50¢");
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
});
