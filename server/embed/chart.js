// The embeddable chart (`/embed/chart?metric=…`): no shell, no nav, one
// chart sized to whatever frames it and readable from across a desk.
// agent-avatar frames it as a screen; the parameters are the strict list in
// docs/API.md "Embed views". The page checks only `theme` itself and hands
// everything else to `GET /api/chart`, which names any parameter it can't
// use, and that message is what this page shows.
//
// Display only, except once: on a server with tokens, the first load shows
// an unlock box; the token is kept in this origin's localStorage. The hidden
// table carries the numbers for a screen reader.
"use strict";

(() => {
  const SVG = "http://www.w3.org/2000/svg";
  const THEMES = ["auto", "light", "dark"];
  const TOKEN_KEY = "vitals.embed.token";
  const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  // Plot margins in px: room for tick labels left and date labels below.
  // Labels are CSS-sized (.em-tick), so these track that size.
  const M = { top: 12, right: 8, bottom: 40, left: 80 };

  const main = document.querySelector("main.em");
  const params = new URLSearchParams(location.search);
  const theme = params.get("theme") ?? "auto";
  params.delete("theme");

  const token = {
    get() {
      try {
        return localStorage.getItem(TOKEN_KEY) ?? "";
      } catch {
        return "";
      }
    },
    set(t) {
      try {
        localStorage.setItem(TOKEN_KEY, t);
      } catch {
        /* private mode: works until reload */
      }
      memo = t;
    },
  };
  let memo = "";

  function el(tag, attrs = {}, ...kids) {
    const n = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (k === "text") n.textContent = v;
      else n.setAttribute(k, v);
    }
    n.append(...kids);
    return n;
  }

  function svg(tag, attrs = {}, text) {
    const n = document.createElementNS(SVG, tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, String(v));
    if (text !== undefined) n.textContent = text;
    return n;
  }

  function show(...kids) {
    main.replaceChildren(...kids);
  }

  function fail(message) {
    document.title = "vitals";
    show(
      el(
        "div",
        { class: "em-error", role: "alert" },
        el("p", { class: "em-kicker", text: "Can't show this chart" }),
        el("p", { class: "em-error-msg", text: message }),
      ),
    );
  }

  // ── numbers ──────────────────────────────────────────────────────────

  const grouped = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });

  function decimals(unit) {
    return unit === "kg" || unit === "lb" || unit === "mL/kg·min" ? 1 : 0;
  }

  /** The big figure and its unit, as separate strings. */
  function figure(v, unit) {
    if (v === null) return ["—", ""];
    if (unit === "h") {
      const m = Math.round(v * 60);
      return [`${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`, ""];
    }
    if (unit === "steps") return [grouped.format(v), "steps"];
    return [v.toFixed(decimals(unit)), unit];
  }

  function signed(v, unit) {
    const [n, u] = figure(Math.abs(v), unit);
    const sign = v > 0 ? "+" : v < 0 ? "−" : "±";
    return `${sign}${n}${u ? ` ${u}` : ""}`;
  }

  function tickLabel(v, unit) {
    if (unit === "h") return `${+v.toFixed(1)}h`;
    if (unit === "steps" && Math.abs(v) >= 1000) return `${+(v / 1000).toFixed(1)}k`;
    return String(+v.toFixed(decimals(unit)));
  }

  /** Round ticks from `lo` to `hi`, about three of them. */
  function ticks(lo, hi) {
    const span = hi - lo || Math.abs(hi) || 1;
    const raw = span / 3;
    const mag = 10 ** Math.floor(Math.log10(raw));
    const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw) ?? raw;
    const out = [];
    for (let t = Math.ceil(lo / step) * step; t <= hi + 1e-9; t += step) out.push(+t.toFixed(6));
    return out;
  }

  function monthOf(key) {
    return MONTHS[Number(key.slice(5, 7)) - 1];
  }

  function dayOf(key) {
    return `${monthOf(key)} ${Number(key.slice(8, 10))}`;
  }

  function keyLabel(view, key) {
    return view.step === "month" ? monthOf(key) : dayOf(key);
  }

  // ── drawing ──────────────────────────────────────────────────────────

  function grid(g, ys, y, w, unit) {
    for (const t of ys) {
      g.append(
        svg("line", {
          class: t === 0 ? "em-base" : "em-grid",
          x1: M.left,
          x2: w - M.right,
          y1: y(t),
          y2: y(t),
        }),
        svg(
          "text",
          { class: "em-tick", x: M.left - 12, y: y(t), dy: "0.35em", "text-anchor": "end" },
          tickLabel(t, unit),
        ),
      );
    }
  }

  /** A bar with its top rounded. */
  function roundedBar(x, y, bw, bh, r) {
    const rr = Math.min(r, bh);
    return `M${x},${y + bh} V${y + rr} Q${x},${y} ${x + rr},${y} H${x + bw - rr} Q${x + bw},${y} ${x + bw},${y + rr} V${y + bh} Z`;
  }

  function bars(g, view, w, h) {
    const pts = view.points;
    const hi = Math.max(0, ...pts.map((p) => p.value ?? 0));
    const ys = ticks(0, hi);
    const top = Math.max(hi, ys[ys.length - 1]);
    const plotH = h - M.top - M.bottom;
    const y = (v) => M.top + ((top - v) / (top || 1)) * plotH;
    grid(g, ys, y, w, view.unit);
    const band = (w - M.left - M.right) / pts.length;
    const gap = Math.max(2, Math.min(band * 0.3, 24));
    const barW = Math.max(2, band - gap);
    const anyLit = pts.some((p) => p.highlighted);
    // A label under every bar up to 12, then every other (or third) one.
    const every = Math.ceil(pts.length / 12);
    const r = Math.min(4, barW / 2);
    // Highlighted bars always get a label; a regular one yields to them
    // when it would sit within a label's width of one.
    // On narrow bars a highlighted run labels only its ends.
    let litAt = pts.flatMap((p, i) => (p.highlighted ? [i] : []));
    if (band < 64 && litAt.length > 1) {
      const [a, b] = [litAt[0], litAt[litAt.length - 1]];
      litAt = b - a >= every ? [a, b] : [a];
    }
    const labelled = (i) =>
      litAt.includes(i) || (i % every === 0 && litAt.every((j) => Math.abs(i - j) >= every));
    pts.forEach((p, i) => {
      const x = M.left + i * band + gap / 2;
      const partial = p.key === view.partial_key;
      // A bucket still in progress keeps a stub outline even at zero.
      if (p.value || partial) {
        const y1 = Math.min(y(p.value ?? 0), y(0) - 4);
        const cls = ["em-bar", anyLit ? (p.highlighted ? "lit" : "dim") : "", partial ? "partial" : ""];
        g.append(svg("path", { class: cls.join(" ").trim(), d: roundedBar(x, y1, barW, y(0) - y1, r) }));
      }
      if (labelled(i)) {
        g.append(
          svg(
            "text",
            {
              class: `em-tick${p.highlighted ? " lit" : ""}`,
              x: x + barW / 2,
              y: h - M.bottom + 26,
              "text-anchor": "middle",
            },
            keyLabel(view, p.key) + (partial ? "*" : ""),
          ),
        );
      }
    });
  }

  function line(g, view, w, h) {
    const pts = view.points;
    const vals = pts.map((p) => p.value);
    let lo = Math.min(...vals);
    let hi = Math.max(...vals);
    const pad = (hi - lo) * 0.12 || Math.abs(hi) * 0.05 || 1;
    lo -= pad;
    hi += pad;
    const ys = ticks(lo, hi).filter((t) => t >= lo && t <= hi);
    const plotH = h - M.top - M.bottom;
    const plotW = w - M.left - M.right;
    const y = (v) => M.top + ((hi - v) / (hi - lo)) * plotH;
    // Points sit at their date, so gaps between readings show as gaps in time.
    const t0 = Date.parse(pts[0].key);
    const t1 = Date.parse(pts[pts.length - 1].key);
    const x = (p) => M.left + (t1 === t0 ? plotW / 2 : ((Date.parse(p.key) - t0) / (t1 - t0)) * plotW);
    const path = (ps) => ps.map((p, k) => `${k ? "L" : "M"}${x(p).toFixed(1)},${y(p.value).toFixed(1)}`).join(" ");
    grid(g, ys, y, w, view.unit);

    const lit = pts.filter((p) => p.highlighted);
    if (lit.length > 1) {
      g.append(
        svg("rect", {
          class: "em-band",
          x: x(lit[0]),
          y: M.top,
          width: Math.max(2, x(lit[lit.length - 1]) - x(lit[0])),
          height: plotH,
        }),
      );
    }
    g.append(svg("path", { class: `em-line${lit.length ? " dim" : ""}`, d: path(pts) }));
    if (lit.length > 1) g.append(svg("path", { class: "em-line lit", d: path(lit) }));
    // Sparse readings (a weigh-in a week) read better as dots too.
    if (pts.length <= 40) {
      for (const p of pts) g.append(svg("circle", { class: "em-pip", cx: x(p), cy: y(p.value), r: 3 }));
    }
    if (lit.length === 1) {
      const p = lit[0];
      g.append(
        svg("line", { class: "em-rule", x1: x(p), x2: x(p), y1: M.top, y2: M.top + plotH }),
        svg("circle", { class: "em-dot", cx: x(p), cy: y(p.value), r: 8 }),
      );
    } else if (!lit.length) {
      const p = pts[pts.length - 1];
      g.append(svg("circle", { class: "em-dot", cx: x(p), cy: y(p.value), r: 6 }));
    }
    // About five date labels, evenly spaced, the last on the last point.
    const n = Math.min(5, pts.length);
    const at = new Set(Array.from({ length: n }, (_, k) => Math.round((k * (pts.length - 1)) / Math.max(1, n - 1))));
    for (const i of at) {
      g.append(
        svg(
          "text",
          {
            class: "em-tick",
            x: x(pts[i]),
            y: h - M.bottom + 26,
            "text-anchor": i === 0 ? "start" : i === pts.length - 1 ? "end" : "middle",
          },
          keyLabel(view, pts[i].key),
        ),
      );
    }
  }

  function plot(view) {
    const box = el("div", { class: "em-plot" });
    const draw = () => {
      const w = box.clientWidth;
      const h = box.clientHeight;
      if (!w || !h) return;
      const s = svg("svg", { width: w, height: h, role: "img", "aria-label": `${view.title}, ${view.range_label}` });
      const g = svg("g");
      s.append(g);
      (view.mark === "bar" ? bars : line)(g, view, w, h);
      box.replaceChildren(s);
    };
    new ResizeObserver(draw).observe(box);
    return box;
  }

  function table(view) {
    const rows = view.points.map((p) => {
      const notes = (p.highlighted ? " (highlighted)" : "") + (p.key === view.partial_key ? " (so far)" : "");
      const [n, u] = figure(p.value, view.unit);
      return el("tr", {}, el("th", { scope: "row", text: p.key + notes }), el("td", { text: `${n} ${u}`.trim() }));
    });
    return el(
      "table",
      { class: "em-sr" },
      el("caption", { text: `${view.title}, ${view.range_label}` }),
      el(
        "thead",
        {},
        el("tr", {}, el("th", { scope: "col", text: view.step }), el("th", { scope: "col", text: view.per })),
      ),
      el("tbody", {}, ...rows),
    );
  }

  function render(view) {
    document.title = `vitals · ${view.title}`;
    const hl = view.headline;
    const [num, unit] = figure(hl.value, view.unit);
    const sub = el("p", { class: "em-sub", text: hl.label });
    if (hl.change !== null) {
      sub.append(
        el("span", {
          class: "em-change",
          text: ` · ${signed(hl.change, view.unit)} since ${dayOf(hl.change_since)}`,
        }),
      );
    }
    const head = el(
      "header",
      { class: "em-head" },
      el(
        "h1",
        { class: "em-kicker" },
        view.title,
        el("span", { class: "em-range", text: ` · ${view.range_label}` }),
      ),
      el("p", { class: "em-figure" }, num, unit ? el("span", { class: "em-unit", text: ` ${unit}` }) : ""),
      sub,
    );
    const hasData = view.points.some((p) => p.value !== null);
    const body = hasData ? plot(view) : el("p", { class: "em-empty", text: "Nothing in this range yet." });
    const foot = el("p", { class: "em-note", text: view.per });
    if (view.partial_key) foot.append(` · * ${keyLabel(view, view.partial_key)} so far`);
    const src = view.sources || [];
    if (src.length === 1) foot.append(` · ${src[0].name}`);
    if (src.length > 1) {
      foot.append(
        ` · ${src.map((x, i) => (i === 0 ? `${x.name} to ${dayOf(x.to)}` : `${x.name} from ${dayOf(x.from)}`)).join(", ")}`,
      );
    }
    show(head, body, foot, table(view));
  }

  // ── auth ─────────────────────────────────────────────────────────────

  function unlock(message) {
    document.title = "vitals";
    const input = el("input", {
      class: "em-input",
      type: "password",
      autocomplete: "off",
      "aria-label": "Read-only API token",
      placeholder: "Read-only API token",
    });
    const form = el(
      "form",
      { class: "em-unlock" },
      el("p", { class: "em-kicker", text: "Unlock vitals" }),
      el("p", { class: "em-error-msg", text: message }),
      input,
      el("button", { class: "em-button", type: "submit", text: "Unlock" }),
    );
    form.addEventListener("submit", (e) => {
      e.preventDefault();
      const t = input.value.trim();
      if (!t) return;
      token.set(t);
      load();
    });
    show(form);
    input.focus();
  }

  async function load() {
    const t = memo || token.get();
    const headers = t ? { Authorization: `Bearer ${t}` } : {};
    let res;
    try {
      res = await fetch(`/api/chart?${params}`, { headers, cache: "no-store" });
    } catch {
      fail("The vitals server didn't answer.");
      return;
    }
    if (res.status === 401 || res.status === 403) {
      unlock(t ? "That token didn't work. Paste the read-only token." : "Paste the read-only token once; this screen keeps it.");
      return;
    }
    let body;
    try {
      body = await res.json();
    } catch {
      fail(`The server sent ${res.status} without a chart.`);
      return;
    }
    if (!res.ok) {
      fail(body.error ?? `The server sent ${res.status}.`);
      return;
    }
    render(body);
  }

  if (!THEMES.includes(theme)) {
    fail("theme: use one of auto, light, dark");
    return;
  }
  main.dataset.theme = theme;
  if (location.pathname.replace(/\/$/, "") !== "/embed/chart") {
    fail("No such embed view. Use /embed/chart.");
    return;
  }
  load();
})();
