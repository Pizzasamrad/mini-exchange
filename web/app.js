import init, { ExchangeDemo } from "./pkg/mini_exchange_wasm.js";

const boot = document.querySelector("#boot");
const bootMsg = document.querySelector("#boot-msg");
const runBtn = document.querySelector("#run");
const runPill = document.querySelector("#run-pill");
const presetPill = document.querySelector("#preset-pill");
const speed = document.querySelector("#speed");
const speedRead = document.querySelector("#speed-read");
const seedInput = document.querySelector("#seed");
const presetSeg = document.querySelector("#preset-seg");
const resetBtn = document.querySelector("#reset");
const booksEl = document.querySelector("#books");
const chart = document.querySelector("#chart");
const legend = document.querySelector("#chart-legend");
const tapeEl = document.querySelector("#tape");
const statGrid = document.querySelector("#stat-grid");
const rateEl = document.querySelector("#rate");
const digestEl = document.querySelector("#digest");
const scriptNote = document.querySelector("#script-note");
const cpFast = document.querySelector("#cp-fast");
const cpRef = document.querySelector("#cp-ref");
const cpState = document.querySelector("#cp-state");
const proveBtn = document.querySelector("#prove");
const proveMsg = document.querySelector("#prove-msg");
const arbPanel = document.querySelector("#arb");
const arbGrid = document.querySelector("#arb-grid");
const ticket = document.querySelector("#ticket");
const sideSeg = document.querySelector("#side-seg");
const marketWrap = document.querySelector("#market-wrap");
const marketSel = document.querySelector("#market");
const priceInput = document.querySelector("#price");
const qtyInput = document.querySelector("#qty");
const cancelId = document.querySelector("#cancel-id");
const liftBtn = document.querySelector("#lift");
const hitBtn = document.querySelector("#hit");
const cancelBtn = document.querySelector("#send-cancel");
const ticketMsg = document.querySelector("#ticket-msg");
const workingEl = document.querySelector("#working");

let session = null;
let preset = "equity";
let running = false;
let raf = 0;
let proving = false;
let lastSnap = null;
let throughput = 0;
let points = [];
let tape = [];
let side = "buy";

function batchSize() {
  const t = Number(speed.value) / 100;
  return Math.max(1, Math.round(400 ** t));
}

function esc(value) {
  return String(value).replace(/[&<>"']/g, (ch) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  }[ch]));
}

function fmtInt(value) {
  if (value == null || Number.isNaN(Number(value))) return "—";
  return Number(value).toLocaleString("en-US");
}

function fmtSigned(value) {
  if (value == null) return "—";
  const n = Number(value);
  const body = Math.abs(n).toLocaleString("en-US");
  if (n > 0) return `+${body}`;
  if (n < 0) return `−${body}`;
  return "0";
}

function pxMain(px) {
  if (px == null) return "—";
  if (preset === "prediction-market") return String(px);
  return Number(px).toLocaleString("en-US");
}

function pxHint(px) {
  if (px == null) return "";
  if (preset === "prediction-market") return `${px}¢`;
  return `$${(px / 100).toFixed(2)}`;
}

function readU32(input, label) {
  const text = input.value.trim();
  if (!/^\d+$/.test(text)) throw new Error(`${label} must be a non-negative integer`);
  const n = Number(text);
  if (!Number.isSafeInteger(n) || n > 0xffffffff) throw new Error(`${label} is too large`);
  return n;
}

function readSeed() {
  const text = seedInput.value.trim();
  if (!/^\d+$/.test(text)) throw new Error("seed must be an integer");
  if (BigInt(text) > 18446744073709551615n) throw new Error("seed does not fit in a u64");
  return text;
}

function selectedMarket(snap) {
  if (marketSel.value) return marketSel.value;
  return snap.markets[0] ? snap.markets[0].symbol : "DEFAULT";
}

function bookFor(snap, symbol) {
  return snap.markets.find((market) => market.symbol === symbol) || snap.markets[0];
}

function syncMarket(snap) {
  const symbols = snap.markets.map((market) => market.symbol);
  const current = [...marketSel.options].map((option) => option.value).join(",");
  if (current !== symbols.join(",")) {
    const previous = marketSel.value;
    marketSel.innerHTML = symbols
      .map((symbol) => `<option value="${esc(symbol)}">${esc(symbol)}</option>`)
      .join("");
    if (symbols.includes(previous)) marketSel.value = previous;
  }
  marketWrap.hidden = symbols.length < 2;
}

function levelRow(kind, symbol, level, maxQty) {
  const width = Math.max(4, Math.round((level.qty / maxQty) * 100));
  const front = level.orders[0];
  const id = front ? front.id : "";
  return `<div class="lvl ${kind}" data-order-id="${esc(id)}" data-market="${esc(symbol)}">
    <span class="px" title="${esc(pxHint(level.price))}">${esc(pxMain(level.price))}</span>
    <span class="bar"><i style="width:${width}%"></i></span>
    <span class="qty">${esc(fmtInt(level.qty))}</span>
    <span class="oid">${id ? `#${esc(id)}` : ""}</span>
  </div>`;
}

function renderBooks(snap) {
  booksEl.classList.toggle("dual", snap.markets.length > 1);
  booksEl.innerHTML = snap.markets
    .map((market) => {
      const levels = [...market.asks, ...market.bids];
      const maxQty = levels.reduce((max, level) => Math.max(max, level.qty), 1);
      const asks = market.asks
        .map((level) => levelRow("ask", market.symbol, level, maxQty))
        .join("");
      const bids = market.bids
        .map((level) => levelRow("bid", market.symbol, level, maxQty))
        .join("");
      const spread = market.spread == null ? "—" : fmtInt(market.spread);
      return `<section class="book">
        <header>
          <h2>${esc(market.symbol)}</h2>
          <div class="touch">
            <span><i>Bid</i><b>${esc(pxMain(market.bestBid))}</b> × ${esc(fmtInt(market.bestBidQty))}</span>
            <span><i>Ask</i><b>${esc(pxMain(market.bestAsk))}</b> × ${esc(fmtInt(market.bestAskQty))}</span>
            <span><i>Last</i><b>${esc(pxMain(market.last))}</b></span>
            <span><i>Spr</i>${esc(spread)}</span>
          </div>
        </header>
        <div class="ladder">
          ${asks || `<div class="empty">no asks</div>`}
          <div class="spread">spread ${esc(spread)}${market.last != null ? ` · last ${esc(pxHint(market.last))}` : ""}</div>
          ${bids || `<div class="empty">no bids</div>`}
        </div>
      </section>`;
    })
    .join("");
}

function stat(label, value, klass) {
  return `<dt>${esc(label)}</dt><dd class="${klass || ""}">${esc(value)}</dd>`;
}

function signedClass(value) {
  const n = Number(value);
  if (n > 0) return "pos";
  if (n < 0) return "neg";
  return "";
}

function renderStats(snap) {
  statGrid.innerHTML = [
    stat("Simulator events", fmtInt(snap.events)),
    stat("Engine commands", fmtInt(snap.commands)),
    stat("Trades", fmtInt(snap.trades)),
    stat("Volume", fmtInt(snap.volume)),
    stat("Rejects", fmtInt(snap.rejects)),
    stat("Manual orders", fmtInt(snap.manualOrders)),
  ].join("");
  rateEl.textContent = throughput > 0 ? fmtInt(Math.round(throughput)) : "—";
  digestEl.textContent = snap.digest;
  if (snap.digestMatchesScript) {
    scriptNote.textContent = `Matches the scripted run of this seed at event ${snap.scriptedEvents}.`;
    scriptNote.className = "hint match";
  } else if (snap.manualOrders > 0) {
    scriptNote.textContent = "Manual orders are part of this tape.";
    scriptNote.className = "hint";
  } else {
    scriptNote.textContent = `Scripted ${snap.scriptedEvents}: ${snap.scriptedDigest}`;
    scriptNote.className = "hint";
  }
  cpFast.textContent = snap.checkpoint.fast;
  cpRef.textContent = snap.checkpoint.reference;
  if (snap.checkpoint.agree) {
    cpState.textContent = "FastEngine and ReferenceEngine agree.";
    cpState.className = "hint match";
  } else {
    cpState.textContent = snap.checkpoint.error || "The engines disagreed.";
    cpState.className = "hint miss";
  }
  if (!snap.arb) {
    arbPanel.hidden = true;
    return;
  }
  arbPanel.hidden = false;
  const arb = snap.arb;
  const edge = arb.avgEdgeTicks == null ? "—" : `${fmtSigned(arb.avgEdgeTicks)} ticks`;
  const win = arb.winRateBps == null ? "—" : `${(arb.winRateBps / 100).toFixed(2)}%`;
  arbGrid.innerHTML = [
    stat("Net PnL", fmtSigned(arb.netPnl), signedClass(arb.netPnl)),
    stat("Realized", fmtSigned(arb.realizedPnl), signedClass(arb.realizedPnl)),
    stat("Unrealized", fmtSigned(arb.unrealizedPnl), signedClass(arb.unrealizedPnl)),
    stat("Equity", fmtInt(arb.equity)),
    stat("Cash", fmtInt(arb.cash)),
    stat("Paired shares", fmtInt(arb.pairedQty)),
    stat("Avg edge", edge),
    stat("Win rate", win),
    stat("Drawdown", `${fmtInt(arb.drawdownTicks)} ticks · ${fmtInt(arb.drawdownBps)} bp`),
    stat("YES / NO", `${fmtSigned(arb.yesNet)} / ${fmtSigned(arb.noNet)}`),
    stat("Actions", fmtInt(arb.actions)),
    stat("Fills", `${fmtInt(arb.fills)} · ${fmtInt(arb.strategyVolume)} sh`),
  ].join("");
}

function renderWorking(snap) {
  if (!snap.working.length) {
    workingEl.innerHTML = "";
    return;
  }
  workingEl.innerHTML = snap.working
    .map(
      (order) => `<div class="work">
        <span class="${order.side}">${esc(order.side)} ${esc(order.market)} ${esc(pxMain(order.price))} × ${esc(fmtInt(order.remaining))}</span>
        <button type="button" data-cancel="${esc(order.id)}" data-market="${esc(order.market)}">#${esc(order.id)}</button>
      </div>`,
    )
    .join("");
}

function renderChrome(snap) {
  document.body.dataset.preset = preset;
  presetPill.textContent = preset;
  runBtn.textContent = running ? "Pause" : "Start";
  runPill.textContent = snap.done ? "End of tape" : running ? "Live" : "Paused";
  runPill.classList.toggle("live", running && !snap.done);
  speedRead.textContent = String(batchSize());
  if (preset === "prediction-market") {
    legend.textContent = "YES  brass    NO  blue";
  } else {
    legend.textContent = "trade print";
  }
}

function render(snap) {
  lastSnap = snap;
  syncMarket(snap);
  renderBooks(snap);
  renderStats(snap);
  renderWorking(snap);
  renderChrome(snap);
  drawChart();
  tapeEl.innerHTML = tape
    .map(
      (print) => `<div class="tape-row ${esc(print.source)}">
        <span class="ts">${esc(print.ts)}</span>
        <span class="mkt">${esc(print.market)}</span>
        <span class="${print.side === "buy" ? "buy" : "sell"}">${print.side === "buy" ? "BUY" : "SELL"}</span>
        <span>${esc(pxMain(print.price))}</span>
        <span>${esc(fmtInt(print.qty))}</span>
        <span class="src">${print.source === "strategy" ? "arb" : esc(print.source)}</span>
      </div>`,
    )
    .join("");
  if (!priceInput.value && document.activeElement !== priceInput) {
    const book = bookFor(snap, selectedMarket(snap));
    if (book && book.bestAsk != null) priceInput.value = String(book.bestAsk);
  }
}

function ingest(snap) {
  for (const print of snap.tradesBatch) {
    points.push(print);
    tape.unshift(print);
  }
  if (points.length > 360) points.splice(0, points.length - 360);
  if (tape.length > 48) tape.length = 48;
}

function drawChart() {
  const rect = chart.getBoundingClientRect();
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const width = Math.max(1, Math.floor(rect.width * dpr));
  const height = Math.max(1, Math.floor(rect.height * dpr));
  if (chart.width !== width || chart.height !== height) {
    chart.width = width;
    chart.height = height;
  }
  const ctx = chart.getContext("2d");
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  const w = rect.width;
  const h = rect.height;
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = "#0c1016";
  ctx.fillRect(0, 0, w, h);
  if (!points.length || w < 10 || h < 10) {
    ctx.fillStyle = "#646c7e";
    ctx.font = "12px IBM Plex Sans, sans-serif";
    ctx.fillText("Trades print here.", 12, 24);
    return;
  }
  const pad = 16;
  const values = points.map((point) => point.price);
  let min = Math.min(...values);
  let max = Math.max(...values);
  if (min === max) {
    min -= 1;
    max += 1;
  }
  const span = max - min;
  const colors = {
    DEFAULT: "#ebe6dc",
    YES: "#d7b56d",
    NO: "#8eb4ff",
  };
  ctx.strokeStyle = "rgba(232, 214, 180, 0.08)";
  ctx.lineWidth = 1;
  for (let i = 1; i <= 3; i += 1) {
    const y = pad + ((h - pad * 2) * i) / 4;
    ctx.beginPath();
    ctx.moveTo(pad, y);
    ctx.lineTo(w - pad, y);
    ctx.stroke();
  }
  const cursor = {};
  points.forEach((point, index) => {
    const x = pad + ((w - pad * 2) * index) / Math.max(points.length - 1, 1);
    const y = pad + ((max - point.price) / span) * (h - pad * 2);
    const prev = cursor[point.market];
    ctx.strokeStyle = colors[point.market] || "#ebe6dc";
    ctx.lineWidth = 1.6;
    ctx.beginPath();
    if (prev) {
      ctx.moveTo(prev.x, prev.y);
      ctx.lineTo(x, y);
    } else {
      ctx.moveTo(x, y);
      ctx.lineTo(x, y);
    }
    ctx.stroke();
    cursor[point.market] = { x, y };
  });
  const last = points[points.length - 1].price;
  const y = pad + ((max - last) / span) * (h - pad * 2);
  ctx.setLineDash([3, 4]);
  ctx.strokeStyle = "rgba(235, 230, 220, 0.35)";
  ctx.beginPath();
  ctx.moveTo(pad, y);
  ctx.lineTo(w - pad, y);
  ctx.stroke();
  ctx.setLineDash([]);
  ctx.fillStyle = "#969eaf";
  ctx.font = "11px IBM Plex Mono, monospace";
  ctx.fillText(pxMain(max), 4, pad + 4);
  ctx.fillText(pxMain(min), 4, h - 4);
}

function applySnap(snap, keepPrints) {
  if (keepPrints) ingest(snap);
  render(snap);
  if (snap.done && running) pause();
}

function tick() {
  raf = 0;
  if (!running || !session) return;
  const started = performance.now();
  const snap = JSON.parse(session.step(batchSize()));
  const elapsed = performance.now() - started;
  if (snap.stepCommands > 0 && elapsed > 0) {
    const instant = (snap.stepCommands * 1000) / elapsed;
    throughput = throughput === 0 ? instant : throughput * 0.82 + instant * 0.18;
  }
  applySnap(snap, true);
  if (running) raf = requestAnimationFrame(tick);
}

function start() {
  if (running || !session || (lastSnap && lastSnap.done)) return;
  running = true;
  if (lastSnap) renderChrome(lastSnap);
  raf = requestAnimationFrame(tick);
}

function pause() {
  running = false;
  if (raf) cancelAnimationFrame(raf);
  raf = 0;
  if (lastSnap) renderChrome(lastSnap);
}

function toggleRun() {
  if (running) pause();
  else start();
}

function rebuild() {
  const seed = readSeed();
  const resume = running;
  pause();
  points = [];
  tape = [];
  throughput = 0;
  priceInput.value = "";
  proveMsg.textContent = "";
  ticketMsg.textContent = "";
  session = new ExchangeDemo(preset, seed);
  seedInput.value = seed;
  const snap = JSON.parse(session.snapshot());
  applySnap(snap, false);
  if (resume && !snap.done) start();
}

function describeCommand(snap) {
  if (snap.error) return snap.error;
  const cmd = snap.lastCommand;
  if (!cmd) return "";
  if (cmd.status === "accepted") {
    return `accepted #${cmd.id} · filled ${fmtInt(cmd.filled)} · resting ${fmtInt(cmd.resting)}`;
  }
  if (cmd.status === "canceled") return `canceled #${cmd.id} · qty ${fmtInt(cmd.canceled)}`;
  return `${cmd.status.replaceAll("_", " ")}${cmd.reason ? ` · ${cmd.reason.replaceAll("_", " ")}` : ""}`;
}

function sendLimit(market, orderSide, price, qty) {
  const snap = JSON.parse(session.limit(market, orderSide, price, qty));
  applySnap(snap, true);
  ticketMsg.textContent = describeCommand(snap);
}

function sendCancel(market, id) {
  const snap = JSON.parse(session.cancel(market, id));
  applySnap(snap, true);
  ticketMsg.textContent = describeCommand(snap);
}

function cross(orderSide) {
  if (!lastSnap) return;
  const preferred = selectedMarket(lastSnap);
  const symbols = [
    preferred,
    ...lastSnap.markets.map((market) => market.symbol).filter((symbol) => symbol !== preferred),
  ];
  const qty = (() => {
    try {
      return readU32(qtyInput, "qty");
    } catch (err) {
      ticketMsg.textContent = err.message;
      return null;
    }
  })();
  if (qty == null) return;
  for (const symbol of symbols) {
    const book = bookFor(lastSnap, symbol);
    const price = orderSide === "buy" ? book.bestAsk : book.bestBid;
    if (price == null) continue;
    marketSel.value = symbol;
    side = orderSide;
    for (const button of sideSeg.querySelectorAll("button")) {
      button.classList.toggle("on", button.dataset.side === side);
    }
    priceInput.value = String(price);
    sendLimit(symbol, orderSide, price, qty);
    return;
  }
  ticketMsg.textContent = orderSide === "buy" ? "no ask is displayed" : "no bid is displayed";
}

async function prove() {
  if (!session || !lastSnap || proving) return;
  proving = true;
  const resume = running;
  pause();
  const frozen = lastSnap;
  proveBtn.disabled = true;
  try {
    if (frozen.events === 0) {
      proveMsg.textContent = "Run the tape, then replay the same seed.";
      return;
    }
    const other = new ExchangeDemo(preset, seedInput.value.trim());
    let left = frozen.events;
    while (left > 0) {
      const chunk = Math.min(left, 2500);
      other.step(chunk);
      left -= chunk;
      proveMsg.textContent = `replaying ${fmtInt(frozen.events - left)} / ${fmtInt(frozen.events)}`;
      await new Promise((resolve) => setTimeout(resolve, 0));
    }
    const fresh = JSON.parse(other.snapshot());
    if (frozen.manualOrders === 0 && fresh.digest === frozen.digest) {
      proveMsg.textContent = `Match. Seed ${frozen.seed} for ${fmtInt(frozen.events)} events prints ${fresh.digest} again.`;
    } else if (frozen.manualOrders > 0) {
      proveMsg.textContent = `Fresh seed digest ${fresh.digest}. The live tape is ${frozen.digest} because it includes ${fmtInt(frozen.manualOrders)} manual orders.`;
    } else {
      proveMsg.textContent = `Differ. Fresh ${fresh.digest}, live ${frozen.digest}.`;
    }
  } catch (err) {
    proveMsg.textContent = err && err.message ? err.message : String(err);
  } finally {
    proveBtn.disabled = false;
    proving = false;
    if (resume && lastSnap && !lastSnap.done) start();
  }
}

runBtn.addEventListener("click", toggleRun);
resetBtn.addEventListener("click", () => {
  try {
    rebuild();
  } catch (err) {
    ticketMsg.textContent = err.message;
  }
});
speed.addEventListener("input", () => {
  speedRead.textContent = String(batchSize());
});
presetSeg.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-preset]");
  if (!button || button.dataset.preset === preset) return;
  preset = button.dataset.preset;
  for (const item of presetSeg.querySelectorAll("button")) {
    item.classList.toggle("on", item === button);
  }
  try {
    rebuild();
  } catch (err) {
    ticketMsg.textContent = err.message;
  }
});
seedInput.addEventListener("keydown", (event) => {
  if (event.key === "Enter") {
    event.preventDefault();
    resetBtn.click();
  }
});

sideSeg.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-side]");
  if (!button) return;
  side = button.dataset.side;
  for (const item of sideSeg.querySelectorAll("button")) {
    item.classList.toggle("on", item === button);
  }
});

ticket.addEventListener("submit", (event) => {
  event.preventDefault();
  if (!lastSnap) return;
  try {
    sendLimit(selectedMarket(lastSnap), side, readU32(priceInput, "price"), readU32(qtyInput, "qty"));
  } catch (err) {
    ticketMsg.textContent = err.message;
  }
});

cancelBtn.addEventListener("click", () => {
  if (!lastSnap) return;
  const id = cancelId.value.trim();
  if (!/^\d+$/.test(id)) {
    ticketMsg.textContent = "enter an order id, or click a level";
    return;
  }
  sendCancel(selectedMarket(lastSnap), id);
});

liftBtn.addEventListener("click", () => cross("buy"));
hitBtn.addEventListener("click", () => cross("sell"));
proveBtn.addEventListener("click", () => {
  prove();
});

booksEl.addEventListener("click", (event) => {
  const row = event.target.closest("[data-order-id]");
  if (!row || !row.dataset.orderId) return;
  cancelId.value = row.dataset.orderId;
  if ([...marketSel.options].some((option) => option.value === row.dataset.market)) {
    marketSel.value = row.dataset.market;
  }
});

workingEl.addEventListener("click", (event) => {
  const button = event.target.closest("button[data-cancel]");
  if (!button) return;
  if ([...marketSel.options].some((option) => option.value === button.dataset.market)) {
    marketSel.value = button.dataset.market;
  }
  sendCancel(button.dataset.market, button.dataset.cancel);
});

window.addEventListener("keydown", (event) => {
  if (event.code !== "Space") return;
  const tag = document.activeElement && document.activeElement.tagName;
  if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA") return;
  event.preventDefault();
  toggleRun();
});

new ResizeObserver(() => {
  if (lastSnap) drawChart();
}).observe(chart);

try {
  await init();
  session = new ExchangeDemo(preset, "1");
  applySnap(JSON.parse(session.snapshot()), false);
  boot.hidden = true;
  start();
} catch (err) {
  bootMsg.textContent = err && err.message ? err.message : String(err);
}
