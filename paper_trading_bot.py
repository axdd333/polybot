#!/usr/bin/env python3
"""
Polymarket Sum-Arb Bot — Paper Trading Mode
$10 starting balance, simulates trading without real money
"""

import asyncio
import random
import time
from datetime import datetime
from aiohttp import web

# ── Configuration ──────────────────────────────────────────────────────────
STARTING_BALANCE = 10.00
MAX_SUM_THRESHOLD = 0.97
MAX_SIZE_PER_SIDE = 0.50
SCAN_INTERVAL_SECS = 2
MIN_ASK_PRICE = 0.05
MAX_ASK_PRICE = 0.95
WEB_PORT = 3000

# ── Simulated Markets ──────────────────────────────────────────────────────
MARKETS = [
    "Will BTC be above $85,000 in 5 min?",
    "Will BTC be above $85,000 in 15 min?",
    "Will ETH be above $3,500 in 5 min?",
    "Will ETH be above $3,500 in 15 min?",
    "Will SOL be above $150 in 5 min?",
    "Will SOL be above $150 in 15 min?",
    "Will Bitcoin price increase in 5 min?",
    "Will Ethereum price increase in 15 min?",
]

# ── Dashboard State ────────────────────────────────────────────────────────
class DashboardState:
    def __init__(self):
        self.total_scans = 0
        self.edges_found = 0
        self.trades_attempted = 0
        self.total_spent = 0.00
        self.markets_checked = 0
        self.recent_markets = []
        self.events = []
        self.started_at = datetime.utcnow().strftime("%Y-%m-%d %H:%M:%S UTC")
        self.balance = STARTING_BALANCE
        self.profit_loss = 0.00

    def push_event(self, level, msg):
        time_str = datetime.now().strftime("%H:%M:%S")
        self.events.insert(0, {"msg": msg, "level": level, "time": time_str})
        if len(self.events) > 100:
            self.events = self.events[:100]

    def push_market(self, check):
        self.recent_markets.insert(0, check)
        if len(self.recent_markets) > 30:
            self.recent_markets = self.recent_markets[:30]

state = DashboardState()

# ── Paper Trading Logic ────────────────────────────────────────────────────
async def paper_scan():
    state.total_scans += 1

    for question in MARKETS:
        state.markets_checked += 1

        # Simulate orderbook prices
        yes_ask = random.uniform(0.30, 0.70)
        no_ask = random.uniform(0.30, 0.70)

        # Clamp to valid range
        yes_ask = max(MIN_ASK_PRICE, min(MAX_ASK_PRICE, yes_ask))
        no_ask = max(MIN_ASK_PRICE, min(MAX_ASK_PRICE, no_ask))

        total_sum = yes_ask + no_ask
        is_edge = total_sum <= MAX_SUM_THRESHOLD

        short_q = question[:57] + "..." if len(question) > 60 else question
        state.push_market({
            "question": short_q,
            "yes_ask": f"{yes_ask:.2f}",
            "no_ask": f"{no_ask:.2f}",
            "sum": f"{total_sum:.4f}",
            "is_edge": is_edge,
            "time": datetime.now().strftime("%H:%M:%S")
        })

        if not is_edge:
            continue

        edge_pct = (1.0 - total_sum) * 100
        print(f"[PAPER] EDGE on {question}: YES=${yes_ask:.2f} + NO=${no_ask:.2f} = ${total_sum:.4f} -> {edge_pct:.1f}% profit")

        state.edges_found += 1
        state.push_event("edge", f"[PAPER] EDGE: {question} YES=${yes_ask:.2f}+NO=${no_ask:.2f}=${total_sum:.4f} ({edge_pct:.1f}%)")

        total_cost = MAX_SIZE_PER_SIDE * yes_ask + MAX_SIZE_PER_SIDE * no_ask

        if total_cost > state.balance:
            state.push_event("warn", f"Insufficient balance! Need ${total_cost:.2f}, have ${state.balance:.2f}")
            continue

        print(f"[PAPER] Executing: {MAX_SIZE_PER_SIDE:.2f} shares each side, total cost ~${total_cost:.2f}")

        # Simulate trade execution
        state.trades_attempted += 1
        state.total_spent += total_cost
        state.balance -= total_cost
        profit = MAX_SIZE_PER_SIDE - total_cost
        state.profit_loss += profit
        state.balance += MAX_SIZE_PER_SIDE  # Simulate resolution payout

        state.push_event("trade", f"[PAPER] SIMULATED ARB! Cost ${total_cost:.2f}, profit ~${profit:.4f}")
        print(f"[PAPER] Simulated arb complete! Cost ~${total_cost:.2f}, profit ~${profit:.4f}")

# ── Scan Loop ──────────────────────────────────────────────────────────────
async def scan_loop():
    print(f"Starting scan loop (every {SCAN_INTERVAL_SECS}s)...")
    while True:
        await paper_scan()
        await asyncio.sleep(SCAN_INTERVAL_SECS)

# ── Web Handlers ──────────────────────────────────────────────────────────
async def serve_dashboard(request):
    html = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Poly Arb Bot — Paper Trading</title>
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    background: #0a0a14;
    color: #d0d0d0;
    font-family: 'SF Mono', 'Fira Code', 'Courier New', monospace;
    font-size: 18px;
    padding: 32px;
    min-height: 100vh;
  }
  .header {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 32px;
  }
  .header h1 {
    font-size: 40px;
    font-weight: 900;
    color: #00ff88;
    letter-spacing: 2px;
  }
  .badge {
    background: #ff6600;
    color: #000;
    font-size: 14px;
    font-weight: 700;
    padding: 4px 12px;
    border-radius: 6px;
    letter-spacing: 1px;
  }
  .pulse {
    width: 14px; height: 14px;
    background: #00ff88;
    border-radius: 50%;
    animation: pulse 2s ease-in-out infinite;
  }
  @keyframes pulse {
    0%, 100% { opacity: 1; box-shadow: 0 0 8px #00ff88; }
    50% { opacity: 0.4; box-shadow: 0 0 2px #00ff88; }
  }
  #status {
    color: #666;
    font-size: 15px;
    margin-bottom: 28px;
  }
  .cards {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
    gap: 16px;
    margin-bottom: 36px;
  }
  .card {
    background: #12122a;
    border: 1px solid #1e1e3a;
    border-radius: 14px;
    padding: 28px 24px;
    text-align: center;
  }
  .card .val {
    font-size: 56px;
    font-weight: 800;
    color: #00ff88;
    line-height: 1;
  }
  .card.warn .val { color: #ff6600; }
  .card.spend .val { color: #00aaff; }
  .card.profit .val { color: #00ff88; }
  .card .lbl {
    font-size: 13px;
    color: #555;
    text-transform: uppercase;
    letter-spacing: 2px;
    margin-top: 10px;
  }
  h2 {
    font-size: 22px;
    color: #00aaff;
    margin: 28px 0 14px;
    letter-spacing: 1px;
  }
  table {
    width: 100%;
    border-collapse: collapse;
    font-size: 17px;
    margin-bottom: 8px;
  }
  th {
    text-align: left;
    padding: 10px 14px;
    color: #444;
    font-size: 12px;
    text-transform: uppercase;
    letter-spacing: 1.5px;
    border-bottom: 1px solid #1a1a2e;
  }
  td {
    padding: 10px 14px;
    border-bottom: 1px solid #111;
  }
  tr.edge td { color: #ff6600; font-weight: 700; }
  tr.no-edge td { color: #444; }
  .events {
    max-height: 420px;
    overflow-y: auto;
    font-size: 16px;
    line-height: 1.8;
  }
  .ev { padding: 2px 0; }
  .ev .ts { color: #333; margin-right: 12px; }
  .ev.info { color: #00aaff; }
  .ev.edge { color: #ff6600; font-weight: 700; }
  .ev.trade { color: #00ff88; font-weight: 700; }
  .ev.warn { color: #ff4444; }
  .empty { color: #333; font-style: italic; padding: 20px 0; }
</style>
</head>
<body>

<div class="header">
  <div class="pulse"></div>
  <h1>POLY ARB BOT</h1>
  <span class="badge">PAPER TRADING</span>
</div>
<div id="status">Connecting...</div>

<div class="cards">
  <div class="card profit"><div class="val" id="balance">$""" + f"{state.balance:.2f}" + """</div><div class="lbl">Balance</div></div>
  <div class="card"><div class="val" id="scans">-</div><div class="lbl">Scans</div></div>
  <div class="card"><div class="val" id="checked">-</div><div class="lbl">Markets Checked</div></div>
  <div class="card warn"><div class="val" id="edges">-</div><div class="lbl">Edges Found</div></div>
  <div class="card"><div class="val" id="trades">-</div><div class="lbl">Simulated Trades</div></div>
  <div class="card spend"><div class="val" id="spent">-</div><div class="lbl">Total Spent</div></div>
</div>

<h2>RECENT MARKET CHECKS</h2>
<table>
  <thead><tr><th>Market</th><th>YES Ask</th><th>NO Ask</th><th>Sum</th><th>Time</th></tr></thead>
  <tbody id="markets"><tr><td colspan="5" class="empty">Waiting for data...</td></tr></tbody>
</table>

<h2>EVENT LOG</h2>
<div class="events" id="events"><div class="empty">No events yet...</div></div>

<script>
async function refresh() {
  try {
    const r = await fetch('/api/stats');
    const d = await r.json();

    document.getElementById('balance').textContent = '$' + d.balance.toFixed(2);
    document.getElementById('scans').textContent = d.total_scans.toLocaleString();
    document.getElementById('checked').textContent = d.markets_checked.toLocaleString();
    document.getElementById('edges').textContent = d.edges_found;
    document.getElementById('trades').textContent = d.trades_attempted;
    document.getElementById('spent').textContent = '$' + d.total_spent.toFixed(2);
    document.getElementById('status').textContent =
      'Paper Trading since ' + d.started_at + '  —  refreshing every 2s';

    if (d.recent_markets.length > 0) {
      let h = '';
      for (const m of d.recent_markets) {
        const c = m.is_edge ? 'edge' : 'no-edge';
        h += '<tr class="' + c + '"><td>' + m.question + '</td><td>$' + m.yes_ask +
             '</td><td>$' + m.no_ask + '</td><td>$' + m.sum + '</td><td>' + m.time + '</td></tr>';
      }
      document.getElementById('markets').innerHTML = h;
    }

    if (d.events.length > 0) {
      let h = '';
      for (const e of d.events) {
        h += '<div class="ev ' + e.level + '"><span class="ts">' + e.time + '</span>' + e.msg + '</div>';
      }
      document.getElementById('events').innerHTML = h;
    }
  } catch(err) {
    document.getElementById('status').textContent = 'Connection lost: ' + err.message;
  }
}
setInterval(refresh, 2000);
refresh();
</script>
</body>
</html>"""
    return web.Response(text=html, content_type='text/html')

async def serve_stats(request):
    return web.json_response({
        "total_scans": state.total_scans,
        "edges_found": state.edges_found,
        "trades_attempted": state.trades_attempted,
        "total_spent": f"{state.total_spent:.2f}",
        "markets_checked": state.markets_checked,
        "recent_markets": state.recent_markets,
        "events": state.events,
        "started_at": state.started_at,
        "balance": state.balance,
        "profit_loss": f"{state.profit_loss:.4f}"
    })

# ── Main ───────────────────────────────────────────────────────────────────
async def main():
    print("╔════════════════════════════════════════════════════════╗")
    print("║  Polymarket Sum-Arb Bot — PAPER TRADING MODE          ║")
    print("║  Strategy: Buy YES+NO when sum <= $0.97               ║")
    print("║  Max risk: $0.50/side, $1.00 total per trade          ║")
    print("║  Starting Balance: $10.00                             ║")
    print("║  Markets: 5/15-min BTC/ETH/SOL                       ║")
    print(f"║  Dashboard: http://localhost:{WEB_PORT}                     ║")
    print("╚════════════════════════════════════════════════════════╝")
    print("")
    print("═══════════════════════════════════════════════════════════")
    print("  PAPER TRADING MODE — No real trades will be executed")
    print("  Simulating market scanning and edge detection")
    print("═══════════════════════════════════════════════════════════")

    app = web.Application()
    app.router.add_get('/', serve_dashboard)
    app.router.add_get('/api/stats', serve_stats)

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, '0.0.0.0', WEB_PORT)
    await site.start()

    print(f"Dashboard live at http://localhost:{WEB_PORT}")

    # Start scan loop
    asyncio.create_task(scan_loop())

    # Keep running
    while True:
        await asyncio.sleep(3600)

if __name__ == '__main__':
    asyncio.run(main())
