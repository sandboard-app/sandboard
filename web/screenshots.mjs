/**
 * Capture the board so it can be looked at, not just reasoned about.
 *
 * Runs a *scratch* sandboard on :8081 against a fixture board, so real state is
 * never touched and the captures are deterministic. Shoots desktop and phone,
 * because §8's whole claim is that the digest is what you read on a phone.
 *
 *   cd web && npm run shots
 *
 * PNGs land in web/shots/ (gitignored).
 *
 * `SANDBOARD_SHOTS_STRICT=1` is for CI, where these PNGs become the docs site's
 * images. Both fallbacks below are correct on a laptop and wrong in a pipeline:
 * skipping on a missing browser publishes a book of broken <img>, and the
 * hand-written mock server serves a snapshot shape that has already drifted
 * from the real one. Under strict mode either is a hard failure — silence is
 * not a passing build.
 */
import { chromium } from "playwright";
import { spawn, execSync } from "node:child_process";
import { createServer } from "node:http";
import { mkdirSync, writeFileSync, copyFileSync, rmSync, existsSync, readFileSync } from "node:fs";
import { setTimeout as sleep } from "node:timers/promises";

const ROOT = new URL("..", import.meta.url).pathname;
const SCRATCH = "/tmp/sandboard-ui";
const OUT = `${ROOT}web/shots`;
const PORT = 8081;
const BASE = `http://127.0.0.1:${PORT}`;
const STRICT = process.env.SANDBOARD_SHOTS_STRICT === "1";

function fatal(why) {
  console.error(`\n[shots] ${why}`);
  process.exit(1);
}

rmSync(SCRATCH, { recursive: true, force: true });
mkdirSync(SCRATCH, { recursive: true });
// Clear OUT too: captures that were renamed away otherwise linger here and get
// swept into the book as if they were current. That is how web/shots ended up
// holding `desktop-overview` and `desktop-tree` long after both were dropped.
rmSync(OUT, { recursive: true, force: true });
mkdirSync(OUT, { recursive: true });

// A scratch sandboard: fixture board in its own state dir. Dispatch starts with the
// process (sandboard.yaml is retired); without OpenShell nothing claims cards, so
// the shoot stays deterministic.
writeFileSync(`${SCRATCH}/sandboard.json`, execSync(`node ${ROOT}web/ui-fixture.mjs`).toString());
mkdirSync(`${SCRATCH}/web`, { recursive: true });
execSync(`cp -R ${ROOT}web/dist ${SCRATCH}/web/dist`);
let sandboard;
if (STRICT && !existsSync(`${ROOT}target/debug/sandboard`)) {
  fatal("target/debug/sandboard is missing — run `cargo build --bin sandboard` first.\nThe mock fallback would publish screenshots of a snapshot shape sandboard no longer serves.");
}
if (existsSync(`${ROOT}target/debug/sandboard`)) {
  sandboard = spawn(`${ROOT}target/debug/sandboard`, [], {
    cwd: SCRATCH,
    env: { ...process.env, SANDBOARD_PORT: String(PORT) },
    stdio: "inherit",
  });
  process.on("exit", () => sandboard.kill());
} else {
  // Lightweight server serving web/dist and fixture data
  const rawData = JSON.parse(readFileSync(`${SCRATCH}/sandboard.json`, "utf8"));
  const snapshotData = JSON.stringify({
    items: Object.values(rawData.items),
    levels: [
      { name: "Vision", horizon: null, owner: null, elaborate: null, requires: [], claimable: false },
      { name: "Project", horizon: null, owner: null, elaborate: null, requires: [], claimable: false },
      { name: "Epic", horizon: null, owner: null, elaborate: null, requires: [], claimable: false },
      { name: "Story", horizon: null, owner: null, elaborate: null, requires: [], claimable: true },
    ],
    goals: [
      {
        id: 1,
        title: "sandboard builds sandboard",
        intent: "sandboard takes cards against its own source and hands back reviewable pull requests.",
        progress: 0.5,
        leaves_done: 4,
        leaves_total: 8,
        agents_live: 3,
        needs_you: 1,
        columns: [],
        story: rawData.stories?.[2] ?? [],
      },
    ],
    server_time: new Date().toISOString(),
    agent_timeout_secs: 600,
    seq: 1,
  });
  const server = createServer((req, res) => {
    if (req.url === "/healthz") {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ status: "ok" }));
    } else if (req.url === "/api/snapshot" || req.url?.startsWith("/api/snapshot")) {
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(snapshotData);
    } else if (req.url?.startsWith("/api/item/")) {
      const id = parseInt(req.url.split("/").pop());
      const item = rawData.items[id];
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(
        JSON.stringify({
          ...item,
          ancestry: [{ level: "Vision", title: "sandboard builds sandboard", intent: "sandboard takes cards" }],
          constraints: [],
          children: [],
        })
      );
    } else {
      let filePath = `${SCRATCH}/web/dist${req.url === "/" ? "/index.html" : req.url}`;
      if (!existsSync(filePath)) filePath = `${SCRATCH}/web/dist/index.html`;
      try {
        const content = readFileSync(filePath);
        const contentType = filePath.endsWith(".css")
          ? "text/css"
          : filePath.endsWith(".js")
          ? "application/javascript"
          : "text/html";
        res.writeHead(200, { "Content-Type": contentType });
        res.end(content);
      } catch {
        res.writeHead(404);
        res.end();
      }
    }
  });
  server.listen(PORT, "127.0.0.1");
  sandboard = { kill: () => server.close() };
}

// Wait for server readiness
for (let i = 0; i < 40; i++) {
  try {
    if ((await fetch(`${BASE}/healthz`)).ok) break;
  } catch {}
  await sleep(250);
}

// A board with no admin serves the bootstrap screen, not the app: `/api/*`
// answers `{"bootstrap":true,"error":"authentication required"}` and every
// capture times out waiting for `.app`. Create a throwaway admin and hand
// Playwright the session cookie. SCRATCH is wiped each run, so this admin
// never outlives the shoot.
const ADMIN = { username: "shots", password: "shots-fixture-only" };
let sessionCookie = null;
try {
  const res = await fetch(`${BASE}/auth/bootstrap`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(ADMIN),
  });
  const setCookie = res.headers.get("set-cookie");
  if (setCookie) {
    const [name, ...rest] = setCookie.split(";")[0].split("=");
    sessionCookie = { name, value: rest.join("="), url: BASE };
  }
} catch {}
if (!sessionCookie) {
  const why = "could not bootstrap a board session; the UI will render its login screen";
  if (STRICT) fatal(why);
  console.log(`[shots] ${why}`);
}

let browser;
try {
  browser = await chromium.launch();
} catch (err) {
  sandboard.kill();
  const why = err.message.split("\n")[0];
  if (STRICT) {
    fatal(`Playwright could not launch chromium: ${why}\nRun \`npx playwright install --with-deps chromium\`.`);
  }
  console.log(`\n[Playwright] Skipping browser screenshots: ${why}`);
  process.exit(0);
}

async function shoot(name, { width, height }, prepare) {
  const context = await browser.newContext({ viewport: { width, height } });
  if (sessionCookie) await context.addCookies([sessionCookie]);
  const page = await context.newPage();
  await page.goto(BASE, { waitUntil: "networkidle" });
  await page.waitForSelector(".app", { timeout: 10_000 });
  await sleep(600);
  if (prepare) await prepare(page);
  await page.screenshot({ path: `${OUT}/${name}.png`, fullPage: true });
  console.log(`  ${name}.png`);
  await context.close();
}

const DESKTOP = { width: 1600, height: 1000 };
const PHONE = { width: 390, height: 844 };

console.log("capturing & asserting:");

// Single board surface (no Home/Board tabs).
await shoot("desktop-board", DESKTOP, async (page) => {
  await page.waitForSelector(".board-page", { timeout: 5000 });

  // Needs you action block when the fixture has an escalation.
  const needs = page.locator(".board-needs");
  if ((await needs.count()) > 0) {
    await needs.first().waitFor({ state: "visible", timeout: 5000 });
    console.log(`  [Playwright Assertion] Needs you block visible`);
  }

  const toggleGraphBtn = page.locator('[data-testid="toggle-graph-view"]');
  if ((await toggleGraphBtn.count()) === 0) {
    await page.locator(".lane-head").first().click();
    await sleep(300);
  }

  // Backlog shows VISIBLE (4) cards and `sortFor("backlog")` sorts blocked ones
  // last, so the dependency chips live behind the "N more" button. Expand it —
  // the chips are the only place the board renders a blocked_by edge, which is
  // worth having in the hero shot.
  const more = page.locator(".column-backlog .chunk").first();
  if ((await more.count()) > 0) {
    await more.click();
    await sleep(300);
  }

  const blockerChips = page.locator(".blocker-chips");
  await blockerChips.first().waitFor({ state: "visible", timeout: 5000 });
  const text = (await blockerChips.first().textContent())?.trim() ?? "";
  console.log(`  [Playwright Assertion] Blocker chips content: "${text}"`);
  // The regression this guards is a chip that degrades to a bare "#2" — a card
  // that tells you it is blocked but not by what. Assert the shape, not a
  // fixture title: pinning the title is what let this assertion rot unnoticed.
  const blockerTitle = text.replace(/⊘|waiting on|#\d+|backlog|running|review|done/g, "").trim();
  if (!text.includes("waiting on") || blockerTitle.length < 10) {
    throw new Error(`Blocker chips are not human-readable. Got: ${text}`);
  }

  const blockedCard = page.locator(".card", { has: page.locator(".blocker-chips") });
  await blockedCard.first().screenshot({ path: `${OUT}/blocked-card-chip.png` });
  console.log(`  blocked-card-chip.png`);
});

await shoot("desktop-graph", DESKTOP, async (page) => {
  const toggleGraphBtn = page.locator('[data-testid="toggle-graph-view"]');
  if ((await toggleGraphBtn.count()) === 0) {
    await page.locator(".lane-head").first().click();
    await sleep(300);
  }
  await page.locator('[data-testid="toggle-graph-view"]').first().click();
  await sleep(600);
  await page.locator('[data-testid="graph-container"]').first().waitFor({
    state: "visible",
    timeout: 5000,
  });
  console.log(`  [Playwright Assertion] Visual dependency graph loaded`);
});

await shoot("phone-board", PHONE);

await shoot("desktop-drawer-needs-you", DESKTOP, async (page) => {
  const card = page.locator(".column-needs_you .card").first();
  if ((await card.count()) === 0) {
    // Open a hot lane, or use the Needs you action title.
    const lane = page.locator(".lane-hot .lane-head").first();
    if ((await lane.count()) > 0) await lane.click();
    await sleep(300);
  }
  if ((await page.locator(".column-needs_you .card").count()) > 0) {
    await page.locator(".column-needs_you .card").first().click();
  } else {
    await page.locator(".board-need-title").first().click();
  }
  await sleep(600);
});

await shoot("desktop-drawer-review", DESKTOP, async (page) => {
  await page.getByRole("button", { name: /Review/ }).first().click();
  await sleep(400);
  const reviewCard = page.locator(".column-review .card").first();
  if ((await reviewCard.count()) === 0) {
    await page.locator(".lane-head").first().click();
    await sleep(300);
  }
  await page.locator(".column-review .card").first().click();
  await sleep(600);
});

// Approve → Tasks is the mechanic the docs tour is built around, so it gets its
// own capture rather than depending on which Review card happens to sort first.
await shoot("desktop-drawer-plan", DESKTOP, async (page) => {
  await page.getByRole("button", { name: /Review/ }).first().click();
  await sleep(400);
  const planCard = page.locator(".column-review .card", { hasText: "Initial Plan for" });
  if ((await planCard.count()) === 0) {
    await page.locator(".lane-head").first().click();
    await sleep(300);
  }
  await page.locator(".column-review .card", { hasText: "Initial Plan for" }).first().click();
  await sleep(600);
  const proposed = page.getByText("Proposed Tasks");
  await proposed.first().waitFor({ state: "visible", timeout: 5000 });
  console.log(`  [Playwright Assertion] Proposed Tasks section visible`);
});

await browser.close();
sandboard.kill();

// A capture that silently did not happen is the failure mode this whole file is
// written against: the book would build green with broken images.
const EXPECTED = [
  "desktop-board",
  "desktop-graph",
  "phone-board",
  "desktop-drawer-needs-you",
  "desktop-drawer-review",
  "desktop-drawer-plan",
  "blocked-card-chip",
];
const missing = EXPECTED.filter((n) => !existsSync(`${OUT}/${n}.png`));
if (missing.length) {
  const why = `missing captures: ${missing.join(", ")}`;
  if (STRICT) fatal(why);
  console.log(`\n[shots] ${why}`);
}

console.log(`\n${OUT}`);
