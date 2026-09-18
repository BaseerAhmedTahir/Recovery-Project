import "./styles.css";
import { call, CliOutput, Device, Filter, HexView, inTauri, listen, Progress, Row } from "./api";
import { escapeHtml, Grid } from "./grid";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;

function fmtBytes(n: number): string {
  const u = ["B", "KiB", "MiB", "GiB", "TiB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${i ? n.toFixed(1) : n} ${u[i]}`;
}

function fmtTime(t: number | null): string {
  return t ? new Date(t * 1000).toISOString().slice(0, 16).replace("T", " ") : "";
}

// ---------------------------------------------------------------------------
// tabs
// ---------------------------------------------------------------------------

document.querySelectorAll<HTMLButtonElement>("nav button").forEach((b) =>
  b.addEventListener("click", () => {
    document.querySelectorAll("nav button").forEach((x) => x.classList.toggle("active", x === b));
    document
      .querySelectorAll<HTMLElement>("main > section")
      .forEach((s) => (s.hidden = s.id !== b.dataset.tab));
  }),
);

if (!inTauri) {
  $("#preview-banner").hidden = false;
}

// ---------------------------------------------------------------------------
// sources and scans
// ---------------------------------------------------------------------------

async function loadDevices() {
  const host = $("#devices");
  try {
    const list = await call<Device[]>("devices");
    host.innerHTML = list.length
      ? list
          .map(
            (d) => `<label class="device ${d.readable ? "" : "unreadable"}">
              <input type="radio" name="device" value="${escapeHtml(d.path)}" ${d.readable ? "" : "disabled"}>
              <b>${escapeHtml(d.path)}</b> ${escapeHtml(d.model ?? "")} · ${fmtBytes(d.size_bytes)} · ${d.kind}
              ${d.removable ? " · removable" : ""}${d.rotational === false ? " · SSD (TRIM may have erased deleted data)" : ""}
              ${d.note ? `<span class="note">${escapeHtml(d.note)}</span>` : ""}
            </label>`,
          )
          .join("")
      : `<p class="muted">No devices listed. ${inTauri ? "Raw devices need the app run as Administrator; image files work without." : ""}</p>`;
    host.querySelectorAll<HTMLInputElement>("input").forEach((i) =>
      i.addEventListener("change", () => ($<HTMLInputElement>("#source").value = i.value)),
    );
  } catch (e) {
    host.innerHTML = `<p class="error">${escapeHtml(String(e))}</p>`;
  }
}
$("#refresh-devices").addEventListener("click", loadDevices);

function setBusy(busy: boolean) {
  $<HTMLButtonElement>("#scan-fs").disabled = busy;
  $<HTMLButtonElement>("#carve").disabled = busy;
  $<HTMLButtonElement>("#stop").disabled = !busy;
}

async function startScan(cmd: "scan_filesystems" | "carve") {
  const source = $<HTMLInputElement>("#source").value.trim();
  if (!source) {
    $("#scan-status").textContent = "Choose a device or type the path of an image file.";
    return;
  }
  setBusy(true);
  $("#scan-notes").innerHTML = "";
  $("#scan-status").textContent = cmd === "carve" ? "Carving…" : "Reading filesystem metadata…";
  try {
    await call(cmd, { source });
  } catch (e) {
    setBusy(false);
    $("#scan-status").textContent = String(e);
  }
}
$("#scan-fs").addEventListener("click", () => startScan("scan_filesystems"));
$("#carve").addEventListener("click", () => startScan("carve"));
$("#stop").addEventListener("click", () => call("stop"));

listen<Progress>("progress", (p) => {
  const bar = $<HTMLProgressElement>("#progress");
  if (p.total > 0) {
    bar.max = p.total;
    bar.value = p.done;
  } else {
    bar.removeAttribute("value"); // indeterminate: the amount of work is not known
  }
  $("#scan-status").textContent = `${p.phase}${
    p.total ? ` — ${fmtBytes(p.done)} of ${fmtBytes(p.total)}` : ""
  } · ${p.found.toLocaleString()} found`;
});

listen<{ ok: boolean; summary?: { rows_added: number; notes: string[] }; error?: string }>(
  "scan-done",
  async (d) => {
    setBusy(false);
    const bar = $<HTMLProgressElement>("#progress");
    bar.max = 1;
    bar.value = d.ok ? 1 : 0;
    if (d.ok && d.summary) {
      $("#scan-status").textContent = `Done: ${d.summary.rows_added.toLocaleString()} rows added.`;
      $("#scan-notes").innerHTML = d.summary.notes.map((n) => `<li>${escapeHtml(n)}</li>`).join("");
      await applyFilter();
    } else {
      $("#scan-status").textContent = `Failed: ${d.error}`;
    }
  },
);

// ---------------------------------------------------------------------------
// results grid
// ---------------------------------------------------------------------------

const grid = new Grid($("#grid"), [
  { key: "band", label: "Rating", width: "84px", render: (r) => (r.band ? `<span class="pill ${r.band.toLowerCase()}">${r.band} ${r.score}</span>` : `<span class="pill">${escapeHtml(r.status)}</span>`) },
  { key: "name", label: "Name", width: "260px" },
  { key: "path", label: "Path", width: "420px" },
  { key: "ext", label: "Type", width: "70px" },
  { key: "size", label: "Size", width: "100px", render: (r) => fmtBytes(r.size) },
  { key: "modified", label: "Modified", width: "140px", render: (r) => fmtTime(r.modified) },
  { key: "kind", label: "Found by", width: "90px" },
  { key: "source", label: "Source", width: "240px" },
]);

let sort: string | null = null;
let descending = false;
document.querySelectorAll<HTMLElement>(".grid-head [data-sort]").forEach((h) =>
  h.addEventListener("click", () => {
    const key = h.dataset.sort!;
    if (sort === key) descending = !descending;
    else {
      sort = key;
      descending = false;
    }
    document.querySelectorAll(".grid-head [data-sort]").forEach((x) => x.classList.remove("asc", "desc"));
    h.classList.add(descending ? "desc" : "asc");
    applyFilter();
  }),
);

function currentFilter(): Filter {
  const checked = (name: string) =>
    [...document.querySelectorAll<HTMLInputElement>(`input[name=${name}]:checked`)].map((i) => i.value);
  const exts = $<HTMLInputElement>("#f-ext")
    .value.split(/[,\s]+/)
    .map((s) => s.trim().toLowerCase().replace(/^\./, ""))
    .filter(Boolean);
  const min = Number($<HTMLInputElement>("#f-min").value);
  return {
    text: $<HTMLInputElement>("#f-text").value.trim(),
    bands: checked("band"),
    kinds: checked("kind"),
    exts,
    min_size: min > 0 ? Math.round(min * 1024) : null,
    sort,
    descending,
  };
}

let filterSeq = 0;
async function applyFilter() {
  const seq = ++filterSeq;
  $("#result-count").textContent = "Filtering…";
  const t0 = performance.now();
  try {
    const n = await call<number>("set_view", { filter: currentFilter() });
    if (seq !== filterSeq) return;
    grid.setCount(n);
    $("#result-count").textContent = `${n.toLocaleString()} rows (${((performance.now() - t0) / 1000).toFixed(1)} s)`;
  } catch (e) {
    $("#result-count").textContent = String(e);
  }
}
let filterTimer = 0;
document.querySelectorAll<HTMLInputElement>(".filters input").forEach((i) =>
  i.addEventListener(i.type === "text" || i.type === "number" ? "input" : "change", () => {
    clearTimeout(filterTimer);
    filterTimer = window.setTimeout(applyFilter, 300);
  }),
);

$("#synthetic").addEventListener("click", async () => {
  const n = Number($<HTMLInputElement>("#synthetic-n").value);
  $("#result-count").textContent = `Adding ${n.toLocaleString()} synthetic rows…`;
  const t0 = performance.now();
  await call("synthetic", { count: n });
  await applyFilter();
  $("#result-count").textContent += ` · added in ${((performance.now() - t0) / 1000).toFixed(1)} s`;
});
$("#clear").addEventListener("click", async () => {
  await call("clear");
  await applyFilter();
});

// ---------------------------------------------------------------------------
// detail: preview, reasons, hex, restore
// ---------------------------------------------------------------------------

let current: Row | null = null;
let hexSkip = 0;
let selection: Row[] = [];

grid.onSelect = (rows) => {
  selection = rows;
  current = rows.length === 1 ? rows[0] : null;
  $("#sel-count").textContent = rows.length ? `${rows.length.toLocaleString()} selected` : "Nothing selected";
  $<HTMLButtonElement>("#restore").disabled = rows.length === 0 || !inTauri;
  const d = $("#detail");
  if (!current) {
    d.innerHTML = rows.length ? `<p class="muted">${rows.length} rows selected.</p>` : `<p class="muted">Select a row.</p>`;
    $("#hex").textContent = "";
    return;
  }
  const r = current;
  d.innerHTML = `
    <h3>${escapeHtml(r.name)}</h3>
    <p class="muted">${escapeHtml(r.path)}</p>
    <dl>
      <dt>Rating</dt><dd>${r.band ? `<span class="pill ${r.band.toLowerCase()}">${r.band} ${r.score}</span>` : escapeHtml(r.status)}</dd>
      <dt>Size</dt><dd>${fmtBytes(r.size)} (${r.size.toLocaleString()} bytes)</dd>
      <dt>Device offset</dt><dd>${r.offset ?? "unknown"}</dd>
      <dt>Source</dt><dd>${escapeHtml(r.source)}</dd>
    </dl>
    <h4>Why this rating</h4>
    <pre class="reasons">${escapeHtml(r.reasons || "—")}</pre>
    <div id="preview-img" class="muted">${inTauri ? "Rendering preview…" : "Previews need the desktop app."}</div>`;
  hexSkip = 0;
  if (inTauri && r.kind !== "synthetic") {
    call<string>("preview", { id: r.id })
      .then((src) => {
        if (current?.id === r.id) $("#preview-img").innerHTML = `<img src="${src}" alt="preview">`;
      })
      .catch((e) => {
        if (current?.id === r.id) $("#preview-img").textContent = `No preview: ${e}`;
      });
    loadHex();
  }
};

async function loadHex() {
  if (!current || !inTauri) return;
  try {
    const v = await call<HexView>("hex", { id: current.id, skip: hexSkip, length: 256 });
    let out = "";
    for (let i = 0; i < v.bytes.length; i += 16) {
      const row = v.bytes.slice(i, i + 16);
      out += `${(v.offset + i).toString(16).padStart(12, "0")}  ${row
        .map((b) => b.toString(16).padStart(2, "0"))
        .join(" ")
        .padEnd(48)}  ${row.map((b) => (b >= 32 && b < 127 ? String.fromCharCode(b) : ".")).join("")}\n`;
    }
    $("#hex").textContent = out;
  } catch (e) {
    $("#hex").textContent = String(e);
  }
}
$("#hex-prev").addEventListener("click", () => {
  hexSkip = Math.max(0, hexSkip - 256);
  loadHex();
});
$("#hex-next").addEventListener("click", () => {
  hexSkip += 256;
  loadHex();
});

$("#restore").addEventListener("click", async () => {
  const out = $<HTMLInputElement>("#restore-out").value.trim();
  if (!out) {
    $("#restore-status").textContent = "Type an output folder on a different disk.";
    return;
  }
  $("#restore-status").textContent = `Writing ${selection.length} files…`;
  try {
    const written = await call<{ dest: string }[]>("restore", {
      ids: selection.map((r) => r.id),
      out,
      reassemble: $<HTMLInputElement>("#restore-reassemble").checked,
    });
    $("#restore-status").textContent = `Wrote ${written.length} files to ${out}, with restore-manifest.json.`;
  } catch (e) {
    $("#restore-status").textContent = `Refused or failed: ${e}`;
  }
});

// ---------------------------------------------------------------------------
// tools: every other CLI capability, through the rc binary
// ---------------------------------------------------------------------------

document.querySelectorAll<HTMLFormElement>("form.tool").forEach((form) =>
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const out = form.querySelector("pre")!;
    const args: string[] = form.dataset.cmd!.split(" ");
    form.querySelectorAll<HTMLInputElement>("input").forEach((i) => {
      if (i.type === "checkbox") {
        if (i.checked) args.push(i.name);
      } else if (i.value.trim()) {
        if (i.name.startsWith("--")) args.push(i.name, i.value.trim());
        else args.push(i.value.trim());
      }
    });
    out.textContent = `rc ${args.join(" ")}\n…`;
    try {
      const r = await call<CliOutput>("run_cli", { args });
      let body = r.stdout;
      try {
        body = JSON.stringify(JSON.parse(r.stdout), null, 2);
      } catch {
        /* not JSON: show as is */
      }
      out.textContent = `rc ${args.join(" ")}  (exit ${r.code})\n${body}${r.stderr ? `\n${r.stderr}` : ""}`;
    } catch (err) {
      out.textContent = String(err);
    }
  }),
);

// Expose for the browser measurement of the grid.
(window as any).rcGrid = grid;

loadDevices();
applyFilter();
