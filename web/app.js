// GlassDB visualizer — vanilla JS, no frameworks.
// The "database server" here is the real Rust engine compiled to WASM;
// everything below is just rendering what it reports.

import init, { WasmDb } from "./pkg/glassdb_wasm.js";

const $ = (id) => document.getElementById(id);

let db = null;
let currentTable = "users";

// One-click guided scenarios for people who don't speak SQL. Each runs a
// real query; `takeaway` turns the engine's stats into plain English.
const SCENARIOS = [
  {
    label: "🔍 Find one person — the fast way",
    sub: "Looks up ID #250 using the database's shortcut",
    sql: "SELECT name, age, city FROM users WHERE id = 250;",
  },
  {
    label: "🐌 Search — the slow way",
    sub: "Searches by age, where no shortcut exists",
    sql: "SELECT name, age FROM users WHERE age = 30;",
  },
  {
    label: "💾 Save a new record — safely",
    sub: "Watch the safety diary work before anything is saved",
    sql: "INSERT INTO users (name, age, city, score) VALUES ('You', 22, 'Ithaca', 99.9);",
  },
  {
    label: "🧠 Ask for the plan first",
    sub: "The engine explains its strategy without doing any work",
    sql: "EXPLAIN SELECT * FROM users WHERE id > 100 AND id <= 120;",
  },
  {
    label: "🤔 Ask an impossible question",
    sub: "It proves no answer can exist — and does zero work",
    sql: "SELECT * FROM users WHERE id > 10 AND id < 5;",
  },
  {
    label: "💥 Make a typo on purpose",
    sub: "Even the error messages point at the exact spot",
    sql: "SELEC name FROM users;",
  },
];

const EXAMPLES = [
  "SELECT name, age, city FROM users WHERE id = 250;",
  "SELECT name, age FROM users WHERE age = 30;",
  "SELECT * FROM users WHERE id > 100 AND id <= 120;",
  "SELECT COUNT(*), AVG(age), MAX(score) FROM users;",
  "SELECT name, score FROM users ORDER BY score DESC LIMIT 5;",
  "INSERT INTO users (name, age, city, score) VALUES ('You', 22, 'Ithaca', 99.9);",
  "UPDATE users SET score = 0.0 WHERE id = 13;",
  "DELETE FROM users WHERE id = 13;",
  "EXPLAIN SELECT * FROM users WHERE id > 10 AND id < 5;",
];

// ---------------------------------------------------------------- boot

async function boot() {
  try {
    await init();
    db = new WasmDb();
    const seeded = JSON.parse(db.seed());
    $("boot-status").textContent = seeded.message
      ? "engine ready · users table seeded (400 rows)"
      : "engine ready";
    $("run").disabled = false;
    $("explain").disabled = false;
    renderScenarios();
    renderExamples();
    runSQL($("sql").value);
  } catch (e) {
    $("boot-status").textContent = "engine failed to load: " + e;
  }
}

function renderScenarios() {
  const wrap = $("scenarios");
  wrap.innerHTML = "";
  for (const sc of SCENARIOS) {
    const b = document.createElement("button");
    b.className = "scenario-btn";
    b.innerHTML = `<strong></strong><span></span>`;
    b.querySelector("strong").textContent = sc.label;
    b.querySelector("span").textContent = sc.sub;
    b.addEventListener("click", () => {
      $("sql").value = sc.sql;
      runSQL(sc.sql);
      $("takeaway").scrollIntoView({ behavior: "smooth", block: "nearest" });
    });
    wrap.appendChild(b);
  }
}

/// How many rows does this table have right now? (Run quietly, after the
/// main query has already been rendered, so it never pollutes the panels.)
function countRows(table) {
  try {
    const res = JSON.parse(db.execute(`SELECT COUNT(*) FROM ${table}`));
    if (res.rows && res.rows.length) return res.rows[0][0];
  } catch {
    /* table may not exist */
  }
  return null;
}

/// Turn the engine's stats into one plain-English sentence or two.
function buildTakeaway(sql, res, total) {
  const s = res.stats || {};
  const isExplain = /^\s*explain\b/i.test(sql);
  const outOf = total !== null ? ` out of ${total}` : "";

  if (isExplain) {
    return (
      "Nothing actually ran — you just asked the engine <em>how</em> it would " +
      "search, like asking a librarian “how would you find this?” before " +
      "sending them off. Its answer is in the strategy panel below."
    );
  }
  if (res.message) {
    // INSERT / UPDATE / DELETE / CREATE …
    let t = `<strong>${res.message}.</strong> `;
    if (s.wal_frames > 0) {
      t +=
        `Before touching the real records, the engine wrote the change into its ` +
        `safety diary (${s.wal_frames} purple ${s.wal_frames === 1 ? "entry" : "entries"} ` +
        `in the play-by-play) and waited for that to be safely on disk. Only then did it ` +
        `update the ${s.pages_written} shelf${s.pages_written === 1 ? "" : "s"}. ` +
        `If the power had been cut at <em>any</em> moment, nothing would be corrupted.`;
    }
    return t;
  }
  const access = res.plan_access ? res.plan_access.type : null;
  const found = res.rows.length;
  if (access === "nothing") {
    return (
      `<strong>Zero work done — on purpose.</strong> The engine looked at your question ` +
      `and <em>proved</em> no record could ever match it (an ID can't be above 10 and ` +
      `below 5 at the same time). So it didn't read a single shelf. ` +
      `Smart laziness is a database superpower.`
    );
  }
  if (access === "pk_lookup" || access === "pk_range") {
    return (
      `<strong>The shortcut worked.</strong> Found ${found} result${found === 1 ? "" : "s"} ` +
      `by checking just ${s.rows_scanned}${outOf} records — it followed the signs in the ` +
      `tree straight to the right shelf (${s.pages_read} shelf visits in total). ` +
      `It would take roughly this same handful of steps even with a million records.`
    );
  }
  return (
    `<strong>No shortcut existed for that question</strong>, so the engine had to check ` +
    `all ${s.rows_scanned}${outOf} records, shelf by shelf — that's why so much of the ` +
    `grid lit up. Same data, different question: searching by ID touches just 2 shelves. ` +
    `That difference is what database indexes are all about.`
  );
}

function renderExamples() {
  const wrap = $("examples");
  wrap.innerHTML = "";
  for (const sql of EXAMPLES) {
    const b = document.createElement("button");
    b.className = "chip";
    b.textContent = sql.length > 52 ? sql.slice(0, 50) + "…" : sql;
    b.title = sql;
    b.addEventListener("click", () => {
      $("sql").value = sql;
      runSQL(sql);
    });
    wrap.appendChild(b);
  }
}

// ---------------------------------------------------------------- run

function tableNameFrom(sql) {
  const m = sql.match(/\b(?:from|into|update|table)\s+([A-Za-z_][A-Za-z0-9_]*)/i);
  return m ? m[1].toLowerCase() : null;
}

function runSQL(sql) {
  if (!db || !sql.trim()) return;
  const res = JSON.parse(db.execute(sql));
  if (res.error) {
    renderError(res.error, sql);
    return;
  }
  $("error").hidden = true;
  currentTable = tableNameFrom(sql) || currentTable;
  renderResults(res);
  renderStats(res.stats);
  $("plan").textContent = res.plan || "(no strategy needed — this wasn't a search or a change)";
  $("plan").classList.remove("muted");
  const visited = animateTrace(res.trace || []);
  renderTree(currentTable, visited);
  // Plain-English explanation of what the engine just did.
  const takeaway = $("takeaway");
  takeaway.innerHTML =
    "<strong>What just happened?</strong> " +
    buildTakeaway(sql, res, countRows(currentTable));
  takeaway.hidden = false;
}

$("run").addEventListener("click", () => runSQL($("sql").value));
$("explain").addEventListener("click", () => {
  const sql = $("sql").value.replace(/^\s*explain\s+/i, "");
  runSQL("EXPLAIN " + sql);
});
$("sql").addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
    e.preventDefault();
    runSQL($("sql").value);
  }
});

// ---------------------------------------------------------------- render

function renderError(err, sql) {
  let text = `${err.kind}: ${err.message}`;
  if (err.line_text !== undefined) {
    text += `\n  line ${err.line}: ${err.line_text}\n  ${" ".repeat(
      String("line " + err.line + ": ").length + err.col - 1
    )}^`;
  }
  const box = $("error");
  box.textContent = text;
  box.hidden = false;
  const takeaway = $("takeaway");
  takeaway.innerHTML =
    "<strong>What just happened?</strong> The engine didn't understand that — " +
    "and instead of guessing, it stopped and pointed at the exact spot where it " +
    "got confused (the red box below, with the little ^ arrow). Nothing was " +
    "touched or changed. Good error messages are a feature, not an accident.";
  takeaway.hidden = false;
}

function renderResults(res) {
  const wrap = $("results");
  wrap.innerHTML = "";
  if (res.message) {
    const p = document.createElement("p");
    p.className = "msg";
    p.textContent = "✓ " + res.message;
    wrap.appendChild(p);
    return;
  }
  const MAX_SHOW = 200;
  const table = document.createElement("table");
  const thead = document.createElement("tr");
  for (const c of res.columns) {
    const th = document.createElement("th");
    th.textContent = c;
    thead.appendChild(th);
  }
  table.appendChild(thead);
  for (const row of res.rows.slice(0, MAX_SHOW)) {
    const tr = document.createElement("tr");
    for (const cell of row) {
      const td = document.createElement("td");
      td.textContent = cell === null ? "NULL" : String(cell);
      if (typeof cell === "number") td.className = "num";
      tr.appendChild(td);
    }
    table.appendChild(tr);
  }
  wrap.appendChild(table);
  const count = document.createElement("p");
  count.className = "rowcount";
  count.textContent =
    res.rows.length > MAX_SHOW
      ? `${res.rows.length} rows (showing first ${MAX_SHOW})`
      : `${res.rows.length} row${res.rows.length === 1 ? "" : "s"}`;
  wrap.appendChild(count);
}

function renderStats(s) {
  if (!s) return;
  $("stats-line").textContent =
    `${s.pages_read} page reads (${s.cache_hits} cached) · ` +
    `${s.pages_written} written · ${s.wal_frames} WAL frames · ` +
    `${s.rows_scanned} rows scanned`;
}

// ------------------------------------------------------- page animation

let animTimer = null;

function animateTrace(events) {
  if (animTimer) clearInterval(animTimer);
  const grid = $("pagegrid");
  const log = $("iolog");
  grid.innerHTML = "";
  log.innerHTML = "";
  log.classList.remove("muted");

  // One cell per page id that shows up anywhere in this statement.
  const pids = [...new Set(events.filter((e) => e.pid !== undefined).map((e) => e.pid))].sort(
    (a, b) => a - b
  );
  const visited = new Set(pids);
  const cells = new Map();
  for (const pid of pids) {
    const cell = document.createElement("div");
    cell.className = "page";
    cell.dataset.pid = "p" + pid;
    grid.appendChild(cell);
    cells.set(pid, cell);
  }
  if (events.length === 0) {
    log.innerHTML = "<div class='ev-note'>no I/O — answered without touching a page</div>";
    return visited;
  }

  const describe = (e) => {
    switch (e.type) {
      case "page_read":
        return [
          e.cached ? "ev-cached" : "ev-read",
          `read  page ${e.pid} [${e.kind}] ${e.cached ? "— buffer pool hit" : "— from disk"}`,
        ];
      case "page_write":
        return ["ev-write", `write page ${e.pid} → database file`];
      case "page_alloc":
        return ["ev-write", `alloc page ${e.pid}`];
      case "page_free":
        return ["ev-write", `free  page ${e.pid}`];
      case "wal_frame":
        return ["ev-wal", `WAL   frame lsn=${e.lsn} (image of page ${e.pid})`];
      case "wal_commit":
        return ["ev-wal", `WAL   COMMIT record lsn=${e.lsn}`];
      case "wal_sync":
        return ["ev-wal", "WAL   fsync — the transaction is now durable"];
      case "wal_checkpoint":
        return ["ev-wal", `WAL   checkpoint: ${e.frames} frames applied, log reset`];
      case "wal_recovery":
        return ["ev-note", `recovery replayed ${e.txns} txns from ${e.frames} frames`];
      default:
        return ["ev-note", e.text || e.type];
    }
  };

  let i = 0;
  const step = Math.min(120, Math.max(10, 2200 / events.length));
  animTimer = setInterval(() => {
    if (i >= events.length) {
      clearInterval(animTimer);
      animTimer = null;
      return;
    }
    const e = events[i++];
    const [cls, text] = describe(e);
    const line = document.createElement("div");
    line.className = cls;
    line.textContent = text;
    log.appendChild(line);
    log.scrollTop = log.scrollHeight;
    if (e.pid !== undefined && cells.has(e.pid)) {
      const cell = cells.get(e.pid);
      cell.classList.remove("read", "cached", "write");
      // force a repaint so repeated hits on the same page re-flash
      void cell.offsetWidth;
      if (e.type === "page_read") cell.classList.add(e.cached ? "cached" : "read");
      else if (e.type === "page_write" || e.type === "page_alloc") cell.classList.add("write");
    }
  }, step);
  return visited;
}

// ------------------------------------------------------------ B+tree svg

function renderTree(table, visited) {
  if (!db) return;
  const wrap = $("tree");
  let layout;
  try {
    layout = JSON.parse(db.layout(table));
  } catch {
    return;
  }
  if (layout.error) {
    wrap.innerHTML = `<p class="muted">no table '${table}' to draw</p>`;
    return;
  }
  $("tree-table").textContent = `— table '${layout.table}', root page ${layout.root}`;

  const NODE_W = 92;
  const NODE_H = 40;
  const GAP_X = 14;
  const GAP_Y = 64;

  // Position leaves left-to-right, parents centered above children.
  let nextLeafX = 0;
  const placed = [];
  const edges = [];
  const leaves = [];

  function place(node, depth) {
    const n = { node, depth, x: 0 };
    if (!node.children || node.children.length === 0) {
      n.x = nextLeafX;
      nextLeafX += NODE_W + GAP_X;
      if (node.kind === "leaf") leaves.push(n);
    } else {
      const kids = node.children.map((c) => place(c, depth + 1));
      n.x = (kids[0].x + kids[kids.length - 1].x) / 2;
      for (const k of kids) edges.push([n, k]);
    }
    placed.push(n);
    return n;
  }
  place(layout.tree, 0);

  const maxDepth = Math.max(...placed.map((n) => n.depth));
  const width = Math.max(nextLeafX - GAP_X, NODE_W);
  const height = (maxDepth + 1) * (NODE_H + GAP_Y) - GAP_Y + 20;
  const y = (d) => d * (NODE_H + GAP_Y) + 10;

  let svg = `<svg viewBox="0 0 ${width} ${height}" width="${Math.min(width, 1200)}" xmlns="http://www.w3.org/2000/svg">`;

  for (const [parent, child] of edges) {
    svg += `<path class="edge" d="M ${parent.x + NODE_W / 2} ${y(parent.depth) + NODE_H}
      C ${parent.x + NODE_W / 2} ${y(parent.depth) + NODE_H + 30},
        ${child.x + NODE_W / 2} ${y(child.depth) - 30},
        ${child.x + NODE_W / 2} ${y(child.depth)}"/>`;
  }
  // Dashed leaf chain: how range scans walk sideways without re-descending.
  for (let i = 0; i + 1 < leaves.length; i++) {
    svg += `<line class="leafchain" x1="${leaves[i].x + NODE_W}" y1="${y(leaves[i].depth) + NODE_H / 2}"
      x2="${leaves[i + 1].x}" y2="${y(leaves[i + 1].depth) + NODE_H / 2}"/>`;
  }

  for (const n of placed) {
    const node = n.node;
    const isVisited = visited && visited.has(node.pid);
    const cls = `node ${node.kind}${isVisited ? " visited" : ""}`;
    const keys = (node.keys || []).slice(0, 3).join(" ");
    const more = node.n_keys > 3 ? "…" : "";
    svg += `<g class="${cls}">
      <rect x="${n.x}" y="${y(n.depth)}" width="${NODE_W}" height="${NODE_H}" rx="5"/>
      <text x="${n.x + 6}" y="${y(n.depth) + 15}" class="kind">p${node.pid} · ${node.kind} · ${node.n_keys ?? "?"} keys</text>
      <text x="${n.x + 6}" y="${y(n.depth) + 31}">${keys}${more}</text>
    </g>`;
  }
  svg += "</svg>";
  wrap.innerHTML = svg;
}

boot();
