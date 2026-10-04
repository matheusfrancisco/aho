// aho web surface. No build step: plain DOM. Everything shown is a fold of entries received over
// /api/mux (branch = walk `parent` links from the newest entry), so a reload or a second tab
// rebuilds the same screen. Only surface-local state lives here: draft, scroll, tab, HUD.
"use strict";

const S = {
  entries: new Map(), tip: null, last: null, ready: false,
  live: { text: "", thinking: "", tools: [] },
  approvals: new Map(), panels: [], tools: {}, tab: null, hud: false,
  inflight: false, turnOpen: false, tree: null, why: null, jobs: [], fold: null,
  rewind: null, dirty: false,
};

const $ = (id) => document.getElementById(id);

function el(tag, attrs, ...kids) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === "class") n.className = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else n.setAttribute(k, v);
  }
  for (const c of kids.flat()) {
    if (c === null || c === undefined || c === false) continue;
    n.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return n;
}

// Replaces `node`'s children, skipping null/undefined/false (optional rows).
function fill(node, ...kids) {
  node.replaceChildren(...kids.flat().filter((k) => k !== null && k !== undefined && k !== false));
}

function notice(text) { $("notice").textContent = text || ""; }

async function api(path, body) {
  const r = await fetch("/api/" + path, {
    method: "POST", credentials: "same-origin",
    headers: { "content-type": "application/json" }, body: JSON.stringify(body || {}),
  });
  if (r.status === 401) { showLogin(); throw new Error("sign in first"); }
  const j = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(j.error || ("HTTP " + r.status));
  return j;
}

// ───────────────────────────── login ─────────────────────────────

function showLogin() { $("login").hidden = false; $("token").focus(); }

$("login-go").addEventListener("click", async () => {
  $("login-err").textContent = "";
  const r = await fetch("/api/auth/exchange", {
    method: "POST", credentials: "same-origin", headers: { "content-type": "application/json" },
    body: JSON.stringify({ token: $("token").value.trim() }),
  });
  const j = await r.json().catch(() => ({}));
  if (!r.ok) { $("login-err").textContent = j.error || ("HTTP " + r.status); return; }
  $("login").hidden = true;
  boot();
});

// ───────────────────────────── the fold ─────────────────────────────

function branch() {
  const out = [];
  let id = S.tip;
  while (id !== null && id !== undefined) {
    const e = S.entries.get(id);
    if (!e) break;
    out.push(e);
    id = e.parent;
  }
  return out.reverse();
}

function who(p) {
  if (!p) return "?";
  if (p.kind === "system") return p.name;
  if (p.kind === "agent") return "agent (turn " + p.turn + ")";
  return p.kind;
}

const DECISION = {
  allowed_once: { once: "approved once", session: "approved for the session", until_revoked: "approved until revoked" },
  rejected: "rejected by the operator", cancelled: "approval cancelled", unavailable: "no one could answer (prompts off)",
};

function decisionText(outcome, scope) {
  const d = DECISION[outcome];
  return typeof d === "string" ? d : (d && d[scope]) || outcome;
}

const CLASS = {
  read_only: "ReadOnly", workspace_write: "WorkspaceWrite", host_write: "HostWrite", network_egress: "NetworkEgress",
  spawns_process: "SpawnsProcess", controls_vm: "ControlsVM", controls_desktop: "ControlsDesktop", talks_to_human: "TalksToHuman",
};
function className(c) { return c ? (CLASS[c] || c) : "unclassified"; }

function modeName(m) { return { both: "both", chat_only: "chat", code_only: "code" }[m] || m; }

function blocks() {
  const out = [], calls = new Map(), asked = new Map();
  let turn = 0, open = false;
  const card = (e, id, name, args) => {
    if (calls.has(id)) return calls.get(id);
    const c = { e, kind: "tool", call: id, tool: name, args, cls: null, op: null, result: null, decision: null };
    calls.set(id, c); out.push(c); return c;
  };
  for (const e of branch()) {
    const k = e.kind;
    switch (k.type) {
      case "turn_start": turn = k.turn; open = true; break;
      case "turn_end": open = false; break;
      case "user_message":
        if (k.form === "prompt") out.push({ e, kind: "user", text: k.text });
        else if (k.form === "notice") out.push({ e, kind: "notice", text: k.text });
        break;
      case "assistant_message":
        for (const b of k.content) {
          if (b.type === "text" && b.text.trim()) out.push({ e, kind: "assistant", text: b.text });
          else if (b.type === "thinking" && b.text.trim()) out.push({ e, kind: "thinking", text: b.text });
          else if (b.type === "tool_use") card(e, b.id, b.name, b.args);
        }
        break;
      case "assistant_attempt": out.push({ e, kind: "error", text: "model request failed: " + k.error }); break;
      case "tool_call": { const c = card(e, k.call, k.tool, k.args); c.tool = k.tool; c.args = k.args; c.cls = k.class; break; }
      case "op_started": { const c = calls.get(k.call); if (c) c.op = k.op; break; }
      case "tool_result": {
        const c = card(e, k.call, "tool", null);
        c.result = { outcome: k.outcome, content: k.content, meta: k.meta, late: k.late, entry: e.id };
        break;
      }
      case "approval_requested":
        if (k.call) { asked.set(e.id, k.call); const c = calls.get(k.call); if (c) c.decision = "approval asked: " + k.reason; }
        break;
      case "approval_decided": { const c = calls.get(asked.get(k.req)); if (c) c.decision = decisionText(k.outcome, k.scope); break; }
      case "workspace_checkpoint": {
        let cause = k.cause.cause.replace("_", " ");
        if (k.cause.cause === "tool_result") { const c = calls.get(k.cause.call); cause = "after " + (c ? c.tool : "call " + k.cause.call); }
        if (k.cause.cause === "pre_rewind") cause = "before rewind";
        out.push({ e, kind: "checkpoint", id: k.id, cause, turn });
        break;
      }
      case "workspace_restored":
        out.push({ e, kind: "restored", text: `↺ rewound to c${k.to} (${modeName(k.mode)}) · ${k.changed.length} changed, ${k.deleted.length} deleted · undo with c${k.from}` });
        break;
      case "step_end":
        if (k.transition && k.transition.transition === "aborted") out.push({ e, kind: "error", text: "turn stopped: " + k.transition.reason });
        break;
      default: break;
    }
  }
  S.turnOpen = open;
  return out;
}

function resultView(r) {
  const text = (r.content || []).filter((b) => b.type === "text").map((b) => b.text).join("\n");
  const o = r.outcome || {};
  if (o.outcome === "err") return { st: "err", summary: `✕ ${o.code}: ${o.reason}`, body: "" };
  if (o.outcome === "cancelled") return { st: "cancelled", summary: `⊘ cancelled (${o.reason})`, body: "" };
  const m = r.meta;
  if (m && m.card === "terminal") return { st: "ok", summary: `✓ $ ${m.command} → exit ${m.exit ?? "?"}`, body: m.output.split("\n").slice(-6).join("\n") };
  if (m && m.card === "diff") return { st: "ok", summary: `✓ edited ${m.path}`, body: m.hunks.join("\n").split("\n").slice(0, 6).join("\n") };
  if (m && m.card === "read") return { st: "ok", summary: `✓ read ${m.path}` + (m.range ? `:${m.range[0]}-${m.range[1]}` : ""), body: "" };
  if (m && m.card === "search") return { st: "ok", summary: `✓ ${m.hits.length} hits for ${m.query}`, body: m.hits.slice(0, 6).join("\n") };
  if (m && m.card === "web") return { st: "ok", summary: `✓ ${m.title} <${m.url}>`, body: "" };
  if (m && m.card === "generic") return { st: "ok", summary: "✓ " + m.title, body: m.body.split("\n").slice(0, 6).join("\n") };
  const lines = text.split("\n");
  return { st: "ok", summary: "✓ " + (lines[0] || "ok"), body: lines.slice(1, 7).join("\n") };
}

function compact(v) {
  if (v === null || v === undefined) return "";
  if (Array.isArray(v)) return "[" + v.map(compact).join(", ") + "]";
  if (typeof v === "object") {
    const ks = Object.keys(v);
    return ks.length ? "{ " + ks.map((k) => k + ": " + compact(v[k])).join(", ") + " }" : "{}";
  }
  const s = JSON.stringify(v);
  return s.length > 200 ? s.slice(0, 199) + "…" : s;
}

// ───────────────────────────── render ─────────────────────────────

function hudNote(b) {
  let t = `↳ e${b.e.id} · by ${who(b.e.by)}`;
  if (b.kind === "tool") {
    t += ` · cap ${S.tools[b.tool] || "?"} · class ${className(b.cls)}`;
    if (b.result) t += ` · result e${b.result.entry}`;
  }
  return el("div", { class: "hud-note" }, t);
}

function renderBlock(b) {
  switch (b.kind) {
    case "tool": {
      const cap = S.tools[b.tool];
      const title = (cap ? cap + "." : "") + b.tool + " · " + className(b.cls);
      let st;
      if (!b.result) st = el("span", { class: "st running" }, "⋯ running" + (b.op !== null ? ` (op ${b.op})` : ""));
      else { const v = resultView(b.result); st = el("span", { class: "st " + v.st }, v.summary + (b.result.late ? " (late)" : "")); b.body = v.body; }
      return el("div", { class: "blk tool", "data-call": b.call },
        el("div", { class: "head" }, "╭ " + title + " ─ ", st),
        b.args !== null && b.args !== undefined ? el("div", { class: "args" }, compact(b.args)) : null,
        b.result && b.body ? el("div", { class: "body" }, b.body) : null,
        b.decision ? el("div", { class: "decision" }, "⚑ " + b.decision) : null,
        hudNote(b));
    }
    case "checkpoint":
      return el("div", { class: "blk checkpoint" },
        el("span", {}, `◆ checkpoint c${b.id} · turn ${b.turn} · ${b.cause}`),
        ...rewindButtons(b.id), hudNote(b));
    case "restored": return el("div", { class: "blk restored" }, b.text, hudNote(b));
    default: return el("div", { class: "blk " + b.kind }, b.text, hudNote(b));
  }
}

function rewindButtons(id) {
  return [["both", "fork the log here and restore the files"], ["chat", "fork the log here; files untouched"], ["code", "restore the files; same conversation, the model is told"]]
    .map(([m, doc]) => el("button", { title: doc, onclick: () => openRewind(id, m) }, m));
}

function render() {
  S.dirty = false;
  const bs = blocks();
  const t = $("transcript");
  const atBottom = t.scrollHeight - t.scrollTop - t.clientHeight < 40;
  fill(t, ...bs.map(renderBlock));
  if (S.live.thinking) t.append(el("div", { class: "blk thinking live" }, S.live.thinking));
  if (S.live.text) t.append(el("div", { class: "blk assistant live" }, S.live.text));
  for (const name of S.live.tools) t.append(el("div", { class: "blk live notice" }, "⋯ calling " + name));
  if (atBottom) t.scrollTop = t.scrollHeight;

  const cps = bs.filter((b) => b.kind === "checkpoint");
  fill($("timeline"), ...(cps.length ? cps.map((b) => el("span", { class: "cp", title: b.cause }, `c${b.id} · turn ${b.turn}`, ...rewindButtons(b.id)))
    : [el("span", { class: "notice" }, "no checkpoints yet")]));

  const busy = S.inflight || S.turnOpen;
  $("status").className = busy ? "busy" : "idle";
  $("status").textContent = busy ? "● running" : "○ idle";
  $("draft").placeholder = busy ? "The turn is running: Enter steers it, Alt+Enter steers urgently." : "Enter sends a prompt, /help lists commands. Shift+Enter for a new line.";
  renderApproval();
}

function schedule() { if (!S.dirty) { S.dirty = true; requestAnimationFrame(render); } }

// ───────────────────────────── approvals ─────────────────────────────

function renderApproval() {
  const m = $("modal");
  if (S.rewind) return;
  const ids = [...S.approvals.keys()].sort((a, b) => a - b);
  if (!ids.length) { m.hidden = true; fill(m); return; }
  const c = S.approvals.get(ids[0]);
  const title = (c.cap ? c.cap + "." : "") + c.tool;
  const world = c.enforced_by ? `${c.world} (${c.enforced_by})` : c.world;
  const keys = [
    el("button", { onclick: () => answer(c.id, "allow_once") }, "[a] allow once"),
    el("button", { onclick: () => answer(c.id, "allow_session") }, `[s] allow for session → opens epoch ${c.epoch_next}`),
    c.suggested_cut ? el("button", { onclick: () => answer(c.id, c.preset_cut ? "preset" : null, c.suggested_cut) }, `[p] aho ${c.suggested_cut}`) : null,
    el("button", { onclick: () => answer(c.id, "reject") }, "[r] reject"),
    el("button", { onclick: () => answer(c.id, "cancel") }, "[Esc] cancel"),
  ];
  fill(m, el("div", { class: "card", role: "dialog", "aria-label": "approval" },
    el("h2", {}, "approval"),
    el("div", { class: "row" }, el("b", {}, title), "    class: ", el("span", { class: "warn" }, className(c.class))),
    el("div", { class: "row" }, "args: " + compact(c.args)),
    el("div", { class: "row" }, `why:  ${c.why}      (reason: ${c.reason})`),
    el("div", { class: "row" }, `world: ${world}  ·  lease: ${c.lease}`),
    el("div", { class: "keys" }, keys),
    ids.length > 1 ? el("div", { class: "row notice" }, `(+${ids.length - 1} more waiting)`) : null));
  m.hidden = false;
}

async function answer(id, outcome, cut) {
  if (!outcome) { notice(`\`${cut}\` is not a preset change; apply it with the aho CLI, then answer this card`); return; }
  try {
    const r = await api("approval/answer", { id, outcome });
    S.approvals.delete(id);
    if (r.wrote) notice(`wrote ${r.wrote.key} = "${r.wrote.value}" to ${r.wrote.file}; ${r.wrote.note}`);
    schedule();
  } catch (e) { notice(e.message); }
}

// ───────────────────────────── rewind ─────────────────────────────

async function openRewind(id, mode) {
  S.rewind = { id, mode, plan: null, error: null };
  drawRewind();
  try { S.rewind.plan = (await api("session/rewind", { checkpoint: id, mode, apply: false })).plan; }
  catch (e) { if (S.rewind) S.rewind.error = e.message; }
  drawRewind();
}

function drawRewind() {
  const m = $("modal");
  const r = S.rewind;
  if (!r) { m.hidden = true; renderApproval(); return; }
  const rows = [];
  if (r.error) rows.push(el("div", { class: "row err" }, "cannot rewind: " + r.error));
  else if (!r.plan) rows.push(el("div", { class: "row notice" }, "computing the plan…"));
  else {
    const p = r.plan;
    rows.push(el("div", { class: "row" }, p.fork_at !== null ? `fork the log at entry ${p.fork_at}` : "same branch: a restore notice is appended for the model"));
    rows.push(el("div", { class: "row" }, p.restore ? `restore files: ${p.restore.changed.length} changed, ${p.restore.deleted.length} deleted  ${[...p.restore.changed, ...p.restore.deleted].slice(0, 8).join(", ")}` : "files untouched"));
    if (p.kill.length) rows.push(el("div", { class: "row" }, `kill ${p.kill.length} running job(s): ${p.kill.join(", ")}`));
    rows.push(el("div", { class: "row notice" }, "a PreRewind checkpoint is taken first, so this rewind can be undone"));
  }
  fill(m, el("div", { class: "card rewind", role: "dialog" },
    el("h2", {}, `rewind to c${r.id}`),
    el("div", { class: "keys" }, ...["both", "chat", "code"].map((mode) => el("button", { class: mode === r.mode ? "on" : "", onclick: () => openRewind(r.id, mode) }, (mode === r.mode ? "▸ " : "") + mode))),
    el("div", { class: "row" }, `plan (${r.mode}):`), ...rows,
    el("div", { class: "keys" },
      el("button", { disabled: !r.plan || undefined, onclick: applyRewind }, "[enter] apply"),
      el("button", { onclick: () => { S.rewind = null; drawRewind(); } }, "[esc] cancel"))));
  m.hidden = false;
}

async function applyRewind() {
  const r = S.rewind;
  if (!r || !r.plan) return;
  S.rewind = null; drawRewind();
  try { const res = await api("session/rewind", { checkpoint: r.id, mode: r.mode, apply: true }); notice(`rewound (entry ${res.entry}); the old branch is kept`); }
  catch (e) { notice("rewind failed: " + e.message); }
}

// ───────────────────────────── sidebar ─────────────────────────────

const TABS = [["sidebar.tree", "Tree"], ["sidebar.inventory", "Inventory"], ["sidebar.jobs", "Jobs"], ["sidebar.stats", "Stats"], ["sidebar.invariants", "Invariants"]];
const STATE_CLASS = { active: "active", unlocked: "unlocked", locked: "locked", broken: "broken", disabled: "disabled" };
const GLYPH = { active: "●", unlocked: "○", locked: "◌", broken: "✕", disabled: "⊘" };

function renderTabs() {
  const slots = new Set(S.panels.map((p) => p.slot));
  const tabs = TABS.filter(([slot]) => slots.has(slot));
  $("sidebar").hidden = tabs.length === 0;
  if (!tabs.some(([s]) => s === S.tab)) S.tab = tabs.length ? tabs[0][0] : null;
  fill($("tabs"), ...tabs.map(([slot, name]) => el("button", { class: slot === S.tab ? "on" : "", onclick: () => { S.tab = slot; renderTabs(); refreshPanel(); } }, name)));
}

function nodeRows(nodes, depth) {
  const out = [];
  for (const n of nodes) {
    const st = n.state.state;
    out.push(el("div", { class: "node", style: `padding-left:${depth * 1.2}em`, onclick: () => showWhy(n.id) },
      el("span", { class: "g " + STATE_CLASS[st] }, GLYPH[st] + " "), n.id, el("span", { class: "st" }, st[0].toUpperCase() + st.slice(1))));
    for (const m of n.missing || []) out.push(el("div", { class: "sub" }, m.requirement + (m.fix ? "\nfix: " + m.fix : "")));
    if (st === "broken") out.push(el("div", { class: "sub err" }, n.state.err));
    if ((st === "unlocked" || st === "disabled") && n.action) out.push(el("div", { class: "sub" }, n.action));
    if (n.over_preset) out.push(el("div", { class: "sub warn" }, "! " + n.over_preset.requirement));
    out.push(...nodeRows(n.children || [], depth + 1));
  }
  return out;
}

async function showWhy(id) {
  try { S.why = await api("caps/why", { id }); } catch (e) { S.why = { error: e.message }; }
  refreshPanel(false);
}

async function refreshPanel(fetch = true) {
  const p = $("panel");
  try {
    if (S.tab === "sidebar.tree") {
      if (fetch || !S.tree) S.tree = await api("caps/tree");
      const why = S.why ? el("pre", { class: "sub" }, S.why.error || JSON.stringify(S.why, null, 2)) : null;
      fill(p, el("div", { class: "notice" }, `plan ${S.tree.plan} · preset ${S.tree.preset} · world ${S.tree.world}`), ...nodeRows(S.tree.nodes, 0), why);
    } else if (S.tab === "sidebar.inventory") {
      const inv = await api("caps/inventory");
      fill(p, ...inv.cards.flatMap((c) => [
        el("div", {}, el("span", { class: "g " + STATE_CLASS[c.state.state] }, GLYPH[c.state.state] + " "), el("b", {}, c.id)),
        el("div", { class: "sub" }, `${c.kind} · ${c.origin}${c.source ? " (" + c.source + ")" : ""} · ${c.state.state}`),
        c.classes.length ? el("div", { class: "sub" }, "consequences: " + c.classes.map(className).join(" · ")) : null,
        c.equip ? el("div", { class: "sub" }, "equip: " + c.equip) : null,
        c.fix ? el("div", { class: "sub warn" }, "fix: " + c.fix) : null,
      ]));
    } else if (S.tab === "sidebar.jobs") {
      const j = (await api("session/jobs")).jobs;
      fill(p, ...(j.length ? j.map((x) => el("div", {}, el("span", { class: "scope " + x.scope }, x.scope), ` op${x.op} ${x.tool}  `,
        el("span", { class: x.state === "running" ? "warn" : x.state === "done" ? "" : "err" },
          x.state === "done" ? `✓ exit ${x.status.code}` : x.state)))
        : [el("div", { class: "notice" }, "no operations yet")]));
    } else if (S.tab === "sidebar.stats") {
      const st = (await api("session/stats")).stats;
      const cache = st.cache_hit_pct === null || st.cache_hit_pct === undefined ? "-" : st.cache_hit_pct + "%";
      fill(p,
        el("div", {}, `turns ${st.turns} · session ${hms(st.duration_ms)}`),
        el("div", {}, `model p50 ${hms(st.model_p50_ms)} · cache ${cache}`),
        el("div", {}, `tokens in ${st.usage.input_tokens} / out ${st.usage.output_tokens}`),
        st.tools.length
          ? el("table", {},
              el("tr", {}, ...["tool", "calls", "ok/err/cancel", "p50"].map((h) => el("th", {}, h))),
              ...st.tools.map((t) => el("tr", {},
                el("td", {}, t.tool), el("td", {}, t.calls),
                el("td", {}, `${t.ok}/${t.errors}/${t.cancelled}`), el("td", {}, hms(t.p50_ms)))))
          : el("div", { class: "notice" }, "no tool calls yet"));
    } else if (S.tab === "sidebar.invariants") {
      const f = await api("session/fold");
      fill(p, ...(f.invariants.length ? f.invariants.map((r) => el("div", {}, r.id + "  ",
        el("span", { class: r.stale ? "warn" : "" }, typeof r.verdict === "string" ? r.verdict : Object.keys(r.verdict)[0]),
        r.stale ? el("div", { class: "sub" }, "code changed since the check") : null))
        : [el("div", { class: "notice" }, "no invariants checked in this session")]));
    }
  } catch (e) { fill(p, el("div", { class: "err" }, e.message)); }
}

function hms(ms) {
  if (ms === null || ms === undefined) return "-";
  if (ms < 1000) return ms + "ms";
  if (ms < 60000) return (ms / 1000).toFixed(1) + "s";
  return Math.floor(ms / 60000) + "m" + String(Math.round((ms % 60000) / 1000)).padStart(2, "0") + "s";
}

// ───────────────────────────── mux ─────────────────────────────

function onFrame(f) {
  if (f.v !== 1) return;
  const p = f.payload;
  switch (f.event) {
    case "entry":
      // Max id, not arrival order: command entries are announced on their own channel and may
      // interleave with live ones; every append moves the head, so the newest id is the tip.
      S.entries.set(p.id, p); if (S.tip === null || p.id >= S.tip) S.tip = p.id; S.last = Math.max(S.last ?? -1, p.id);
      if (p.kind.type === "assistant_message") S.live = { text: "", thinking: "", tools: [] };
      if (S.ready && ["cap_delta", "policy_epoch"].includes(p.kind.type)) { S.tree = null; loadPanels(); }
      if (S.ready && S.tab === "sidebar.jobs" && ["op_started", "op_done", "op_detached", "job_delivered", "tool_result"].includes(p.kind.type)) refreshPanel();
      if (S.ready && S.tab === "sidebar.stats" && ["tool_result", "turn_end"].includes(p.kind.type)) refreshPanel();
      break;
    case "stream":
      if (p.kind === "text") S.live.text += p.text;
      else if (p.kind === "thinking") S.live.thinking += p.text;
      else if (p.kind === "tool_use_start") S.live.tools.push(p.name);
      break;
    case "idle": S.live = { text: "", thinking: "", tools: [] }; S.inflight = false; break;
    case "error": notice(p.message); break;
    case "approval": S.approvals.set(p.id, p); break;
    case "job_finished": if (S.tab === "sidebar.jobs") refreshPanel(); break;
    case "approval_resolved": S.approvals.delete(p.id); break;
    case "ready": S.ready = true; break;
    default: break;
  }
  schedule();
}

function connect() {
  const proto = location.protocol === "https:" ? "wss://" : "ws://";
  const ws = new WebSocket(proto + location.host + "/api/mux" + (S.last !== null ? "?from=" + S.last : ""));
  ws.onmessage = (m) => { try { onFrame(JSON.parse(m.data)); } catch (e) { notice("bad frame: " + e.message); } };
  ws.onclose = () => { S.ready = false; setTimeout(connect, 1000); };
}

// ───────────────────────────── input ─────────────────────────────

// `/name args…` runs one of the server's slash commands (`/help` lists them) and never reaches
// the model; `//text` sends the prompt `/text`.
async function submit(urgent) {
  const d = $("draft");
  let text = d.value.trim();
  if (!text) return;
  d.value = "";
  if (text.startsWith("/") && !text.startsWith("//")) {
    try { const r = await api("session/command", { line: text }); notice(r.output); } catch (e) { notice(e.message); }
    return;
  }
  if (text.startsWith("//")) text = text.slice(1);
  try {
    if (urgent || S.inflight || S.turnOpen) { await api("session/steer", { text, urgent: !!urgent }); notice(urgent ? "urgent steer queued" : "steer queued"); return; }
    S.inflight = true; schedule();
    const r = await api("session/prompt", { text });
    if (r.transition && r.transition.transition !== "completed") notice("turn ended: " + JSON.stringify(r.transition));
  } catch (e) {
    if (/already running/.test(e.message)) { await api("session/steer", { text, urgent: false }).catch(() => {}); notice("a turn was already running; your text was queued as a steer"); }
    else notice(e.message);
  } finally { S.inflight = false; schedule(); }
}

$("composer").addEventListener("submit", (ev) => { ev.preventDefault(); submit(false); });
$("draft").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter" && !ev.shiftKey) { ev.preventDefault(); submit(ev.altKey); }
});
$("abort").addEventListener("click", () => api("session/abort").then(() => notice("abort requested")).catch((e) => notice(e.message)));
$("hud-toggle").addEventListener("click", () => document.body.classList.toggle("hud"));

document.addEventListener("keydown", (ev) => {
  if (ev.altKey && (ev.key === "h" || ev.key === "˙")) { ev.preventDefault(); document.body.classList.toggle("hud"); return; }
  if (S.rewind) {
    if (ev.key === "Escape") { S.rewind = null; drawRewind(); }
    else if (ev.key === "Enter") { ev.preventDefault(); applyRewind(); }
    return;
  }
  const ids = [...S.approvals.keys()].sort((a, b) => a - b);
  if (!ids.length || ev.target === $("token")) return;
  const c = S.approvals.get(ids[0]);
  const map = { a: "allow_once", s: "allow_session", r: "reject", Escape: "cancel" };
  if (map[ev.key]) { ev.preventDefault(); answer(c.id, map[ev.key]); }
  else if (ev.key === "p" && c.suggested_cut) { ev.preventDefault(); answer(c.id, c.preset_cut ? "preset" : null, c.suggested_cut); }
});

// ───────────────────────────── boot ─────────────────────────────

async function loadPanels() {
  const r = await api("ui/panels");
  S.panels = r.panels; S.tools = r.tools || {};
  $("chat").hidden = !S.panels.some((p) => p.slot === "chat");
  if ($("chat").hidden) notice("no Active capability provides the chat panel");
  renderTabs();
  refreshPanel();
  const f = await api("session/fold");
  $("title").textContent = "aho · session " + f.session.slice(0, 8);
  $("policy").textContent = `preset ${f.policy.preset} (epoch ${f.policy.epoch})`;
}

async function boot() {
  try { await loadPanels(); } catch (e) { if (!$("login").hidden) return; notice(e.message); return; }
  connect();
  setInterval(() => { if (S.tab === "sidebar.jobs" || S.tab === "sidebar.stats") refreshPanel(); }, 2000);
}

boot();
