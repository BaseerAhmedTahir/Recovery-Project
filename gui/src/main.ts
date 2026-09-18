import "./styles.css";
import {
  call,
  CliOutput,
  Device,
  Filter,
  bridgeError,
  onBackendKnown,
  inTauri,
  listen,
  pickFolder,
  pickImageFile,
  Progress,
  Row,
  Written,
} from "./api";
import { escapeHtml, ResultsView } from "./grid";

const $ = <T extends HTMLElement = HTMLElement>(sel: string) => document.querySelector(sel) as T;
const all = <T extends HTMLElement = HTMLElement>(sel: string) => [...document.querySelectorAll<T>(sel)];
const icon = (name: string) => `<svg class="i"><use href="#i-${name}" /></svg>`;

function bytes(n: number): string {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${i === 0 ? n : n.toFixed(n < 10 ? 1 : 0)} ${u[i]}`;
}

function when(t: number | null): string {
  return t ? new Date(t * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }) : "";
}

/** GREEN/YELLOW/RED and validation status, said in words. */
function health(r: Row): { cls: string; label: string } {
  if (r.band === "GREEN" || r.status === "valid") return { cls: "ok", label: "Looks intact" };
  if (r.band === "RED") return { cls: "bad", label: "Likely damaged" };
  if (r.band === "YELLOW" || r.status === "partial") return { cls: "warn", label: "May be damaged" };
  return { cls: "plain", label: "Not checked" };
}

const CATEGORIES: Record<string, string[]> = {
  photos: ["jpg", "jpeg", "png", "heic", "heif", "gif", "bmp", "tif", "tiff", "webp", "dng", "cr2", "cr3", "nef", "arw", "raf", "orf", "rw2", "mp4", "mov", "m4v", "avi", "mkv", "3gp", "webm", "mts"],
  documents: ["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "odt", "ods", "csv", "sqlite", "db", "eml", "msg", "epub"],
  audio: ["mp3", "wav", "flac", "m4a", "aac", "ogg", "opus", "wma", "amr"],
  everything: [],
};
const PICTURE_EXTS = new Set(["jpg", "jpeg", "png", "heic", "heif", "gif", "bmp", "webp", "tif", "tiff", "mp4", "mov", "m4v", "avi", "mkv", "webm", "3gp"]);

const state = {
  kind: "everything" as keyof typeof CATEGORIES | "phone",
  source: "",
  sourceLabel: "",
  category: "everything",
  search: "",
  tiles: true,
  folder: "",
  scanning: false,
};

// ---------------------------------------------------------------------------
// steps
// ---------------------------------------------------------------------------

const STEPS = [
  { id: "w-what", label: "What" },
  { id: "w-where", label: "Where" },
  { id: "w-scan", label: "Scan" },
  { id: "w-results", label: "Choose" },
  { id: "w-save", label: "Recover" },
];

function show(id: string) {
  all<HTMLElement>("main > section").forEach((s) => (s.hidden = s.id !== id));
  const index = STEPS.findIndex((s) => s.id === id);
  const stepper = $("#stepper");
  stepper.hidden = index < 0;
  if (index >= 0) {
    stepper.innerHTML = STEPS.map(
      (s, i) =>
        `<div class="step ${i < index ? "done" : i === index ? "now" : ""}">
           <span class="num">${i < index ? "✓" : i + 1}</span>${s.label}
         </div>${i < STEPS.length - 1 ? '<span class="step-sep"></span>' : ""}`,
    ).join("");
  }
  $("#to-advanced").hidden = id === "adv";
  if (id === "w-results") results.relayout();
}

// ---------------------------------------------------------------------------
// step 1: what
// ---------------------------------------------------------------------------

all<HTMLButtonElement>(".choice").forEach((b) =>
  b.addEventListener("click", () => {
    state.kind = b.dataset.kind as typeof state.kind;
    if (state.kind === "phone") {
      show("w-phone");
      return;
    }
    state.category = state.kind;
    all(".toolbar .chip").forEach((c) => c.setAttribute("aria-pressed", String(c.getAttribute("data-cat") === state.category)));
    $("#where-sub").textContent =
      state.kind === "photos"
        ? "Choose the drive, memory card or phone card the pictures were on."
        : "Choose the drive, memory card or USB stick to search.";
    show("w-where");
    loadDevices();
  }),
);

// ---------------------------------------------------------------------------
// step 2: where
// ---------------------------------------------------------------------------

function driveName(d: Device): string {
  if (d.model && d.model.trim()) return d.model.trim();
  if (d.removable) return "Removable drive";
  return d.kind === "Image" ? "Disk image file" : "Disk";
}

async function loadDevices() {
  const host = $("#device-list");
  host.innerHTML = `<div class="row-item"><div class="spinner"></div><div class="grow muted">Looking for drives…</div></div>`;
  const notice = $("#elevation-notice");
  notice.innerHTML = "";
  try {
    const elevated = await call<boolean>("elevation");
    const devices = await call<Device[]>("devices");
    if (!elevated && inTauri()) {
      notice.innerHTML = `<div class="notice warn">${icon("shield")}
        <div><b>Some drives need Administrator.</b> Windows only lets a program read a whole
        disk with Administrator rights. Disk image files work without.</div>
        <button id="elevate" class="primary">Restart as Administrator</button></div>`;
      $("#elevate")?.addEventListener("click", async () => {
        try {
          await call("restart_elevated");
        } catch (e) {
          notice.innerHTML = `<div class="notice bad">${icon("warn")}<div>${escapeHtml(String(e))}</div></div>`;
        }
      });
    }
    if (!devices.length) {
      host.innerHTML = `<div class="row-item"><div class="grow muted">No drives were found. Plug the card or USB stick in and press Refresh.</div></div>`;
      return;
    }
    host.innerHTML = devices
      .map(
        (d, i) => `<button class="row-item ${d.readable ? "" : "disabled"}" data-i="${i}" ${d.readable ? "" : "disabled"}>
          ${icon(d.removable ? "card" : "drive")}
          <span class="grow">
            <b>${escapeHtml(driveName(d))}</b>
            <small>${bytes(d.size_bytes)} · ${escapeHtml(d.path)}${d.removable ? " · removable" : ""}${
              d.rotational === false && !d.removable ? " · SSD" : ""
            }</small>
          </span>
          ${d.readable ? '<span class="badge plain">Choose</span>' : `<span class="badge warn">${escapeHtml(d.note ?? "not readable")}</span>`}
        </button>`,
      )
      .join("");
    host.querySelectorAll<HTMLButtonElement>("[data-i]").forEach((b) =>
      b.addEventListener("click", () => {
        const d = devices[Number(b.dataset.i)];
        startScan(d.path, `${driveName(d)} · ${bytes(d.size_bytes)}`);
      }),
    );
  } catch (e) {
    host.innerHTML = `<div class="row-item"><div class="grow" style="color:var(--bad)">${escapeHtml(String(e))}</div></div>`;
  }
}

$("#refresh-devices").addEventListener("click", loadDevices);
$("#where-back").addEventListener("click", () => show("w-what"));
$("#pick-image").addEventListener("click", async () => {
  const p = await pickImageFile();
  if (p) startScan(p, p);
});

// ---------------------------------------------------------------------------
// step 3: scan
// ---------------------------------------------------------------------------

async function startScan(source: string, label: string) {
  foundBefore = 0;
  state.source = source;
  state.sourceLabel = label;
  state.scanning = true;
  show("w-scan");
  $("#scan-source").textContent = label;
  $("#scan-title").textContent = "Looking for your files…";
  $("#scan-phase").textContent = "Starting…";
  $("#scan-found").textContent = "0 found";
  $("#scan-notes").innerHTML = "";
  $("#scan-spin").hidden = false;
  $("#scan-results").hidden = true;
  $<HTMLButtonElement>("#scan-stop").disabled = false;
  $("#scan-bar").classList.add("indeterminate");
  try {
    // Each scan in the simple view shows its own results only; results from
    // an earlier scan (or an earlier run of the app) would look like
    // duplicates. The advanced view can still pile scans together.
    await call("clear");
    thumbs.clear();
    await call("start_recovery", { source, deep: $<HTMLInputElement>("#deep").checked });
  } catch (e) {
    state.scanning = false;
    $("#scan-phase").textContent = String(e);
    $("#scan-spin").hidden = true;
  }
}

$("#scan-stop").addEventListener("click", () => {
  $("#scan-phase").textContent = "Stopping…";
  call("stop");
});
$("#scan-results").addEventListener("click", () => openResults());

// The deep scan reports its own count from zero; the files the filesystem
// phase already found must not appear to vanish.
let foundBefore = 0;
listen<number>("rows-added", (n) => {
  foundBefore = n;
});

listen<Progress>("progress", (p) => {
  const bar = $("#scan-bar");
  const fill = bar.querySelector("i") as HTMLElement;
  if (p.total > 0) {
    bar.classList.remove("indeterminate");
    fill.style.width = `${Math.min(100, (p.done / p.total) * 100).toFixed(1)}%`;
    $("#scan-phase").textContent = `${p.phase} — ${bytes(p.done)} of ${bytes(p.total)}`;
  } else {
    bar.classList.add("indeterminate");
    $("#scan-phase").textContent = p.phase;
  }
  $("#scan-found").textContent = `${(foundBefore + p.found).toLocaleString()} found`;
  // The advanced panel shares the same events.
  $("#adv-status").textContent = `${p.phase} · ${p.found.toLocaleString()} found`;
  const advBar = $("#adv-bar");
  advBar.style.width = p.total ? `${((p.done / p.total) * 100).toFixed(1)}%` : "40%";
});

listen<{ ok: boolean; summary?: { rows_added: number; notes: string[] }; error?: string }>(
  "scan-done",
  async (d) => {
    state.scanning = false;
    $("#scan-spin").hidden = true;
    $<HTMLButtonElement>("#scan-stop").disabled = true;
    $<HTMLButtonElement>("#adv-stop").disabled = true;
    $("#scan-bar").classList.remove("indeterminate");
    ($("#scan-bar").querySelector("i") as HTMLElement).style.width = "100%";
    if (!d.ok) {
      $("#scan-title").textContent = "The scan could not finish";
      $("#scan-phase").textContent = d.error ?? "";
      $("#adv-status").textContent = d.error ?? "";
      return;
    }
    const notes = d.summary?.notes ?? [];
    $("#scan-notes").innerHTML = notes.map((n) => `<li>${escapeHtml(n)}</li>`).join("");
    $("#adv-notes").innerHTML = $("#scan-notes").innerHTML;
    $("#scan-title").textContent = "Finished searching";
    $("#scan-phase").textContent = `${(foundBefore + (d.summary?.rows_added ?? 0)).toLocaleString()} files found`;
    $("#scan-results").hidden = false;
    if (!$("#w-scan").hidden) openResults();
    if (!$("#adv").hidden) applyAdvancedFilter();
  },
);

// ---------------------------------------------------------------------------
// step 4: results
// ---------------------------------------------------------------------------

const thumbs = new Map<number, string | null>();
const asking = new Set<number>();

function thumbFor(row: Row): string {
  if (!PICTURE_EXTS.has(row.ext)) return `<div class="thumb">${icon("file")}</div>`;
  const got = thumbs.get(row.id);
  if (got) return `<div class="thumb"><img src="${got}" alt="" loading="lazy"></div>`;
  if (got === null) return `<div class="thumb">${icon("photo")}</div>`;
  if (!asking.has(row.id)) {
    asking.add(row.id);
    call<string | null>("thumbnail", { id: row.id })
      .then((src) => thumbs.set(row.id, src))
      .catch(() => thumbs.set(row.id, null))
      .finally(() => {
        asking.delete(row.id);
        results.refresh();
      });
  }
  return `<div class="thumb">${icon("photo")}</div>`;
}

function tile(row: Row | undefined, _index: number, selected: boolean): string {
  if (!row) return `<div class="tile"><div class="thumb"></div><div class="meta"><span class="name">…</span></div></div>`;
  const h = health(row);
  return `<div class="tile ${selected ? "selected" : ""}">
    ${thumbFor(row)}
    <span class="pick">${selected ? icon("check") : ""}</span>
    <div class="meta">
      <span class="name" title="${escapeHtml(row.path)}">${escapeHtml(row.name)}</span>
      <span class="sub"><span>${bytes(row.size)}</span><span class="badge ${h.cls}">${h.label}</span></span>
    </div>
  </div>`;
}

function listRow(row: Row | undefined, _index: number, selected: boolean): string {
  if (!row) return `<div class="vrow"><span class="name muted">…</span></div>`;
  const h = health(row);
  return `<div class="vrow ${selected ? "selected" : ""}" style="height:100%">
    <input type="checkbox" ${selected ? "checked" : ""} tabindex="-1" />
    ${icon(PICTURE_EXTS.has(row.ext) ? "photo" : "file")}
    <span class="name" title="${escapeHtml(row.path)}">${escapeHtml(row.name)}</span>
    <span class="where">${escapeHtml(row.path)}</span>
    <span class="size">${bytes(row.size)}</span>
    <span class="size">${when(row.modified)}</span>
    <span class="badge ${h.cls}">${h.label}</span>
  </div>`;
}

const results = new ResultsView($("#results"), {
  lineHeight: 190,
  perLine: 5,
  render: tile,
  emptyHtml: `${icon("empty")}<b>Nothing here yet</b><span class="faint">Run a scan, or widen the filter.</span>`,
  onSelectionChange: (n) => {
    $("#selection").textContent = n ? `${n.toLocaleString()} selected` : "Nothing selected";
    $<HTMLButtonElement>("#recover").disabled = n === 0;
    $("#save-n").textContent = n.toLocaleString();
  },
});

function tilesPerLine(): number {
  const w = $("#results").clientWidth || 1200;
  return Math.max(2, Math.min(8, Math.floor(w / 210)));
}

function applyShape() {
  if (state.tiles) results.setShape(190, tilesPerLine(), tile);
  else results.setShape(40, 1, listRow);
  $("#view-tiles").classList.toggle("primary", state.tiles);
  $("#view-list").classList.toggle("primary", !state.tiles);
}

async function openResults() {
  show("w-results");
  await applyFilter();
}

async function applyFilter() {
  const filter: Filter = {
    text: state.search,
    bands: [],
    kinds: [],
    exts: CATEGORIES[state.category] ?? [],
    min_size: null,
    sort: "size",
    descending: true,
  };
  $("#count").textContent = "Sorting…";
  const n = await call<number>("set_view", { filter });
  thumbs.clear();
  results.setCount(n);
  applyShape();
  $("#count").textContent = `${n.toLocaleString()} file${n === 1 ? "" : "s"}`;
}

all(".toolbar .chip").forEach((c) =>
  c.addEventListener("click", () => {
    state.category = c.getAttribute("data-cat")!;
    all(".toolbar .chip").forEach((x) => x.setAttribute("aria-pressed", String(x === c)));
    state.tiles = state.category === "photos";
    applyFilter();
  }),
);
let searchTimer = 0;
$("#search").addEventListener("input", (e) => {
  state.search = (e.target as HTMLInputElement).value.trim();
  clearTimeout(searchTimer);
  searchTimer = window.setTimeout(applyFilter, 250);
});
$("#view-tiles").addEventListener("click", () => {
  state.tiles = true;
  applyShape();
});
$("#view-list").addEventListener("click", () => {
  state.tiles = false;
  applyShape();
});
$("#select-all").addEventListener("click", () => results.selectAll());
$("#select-none").addEventListener("click", () => results.clearSelection());
$("#results-back").addEventListener("click", () => show("w-where"));
$("#recover").addEventListener("click", () => show("w-save"));
window.addEventListener("resize", () => {
  if (!$("#w-results").hidden && state.tiles) applyShape();
});

// ---------------------------------------------------------------------------
// step 5: save
// ---------------------------------------------------------------------------

$("#choose-folder").addEventListener("click", async () => {
  const folder = await pickFolder();
  if (!folder) return;
  state.folder = folder;
  $("#folder").textContent = folder;
  $<HTMLButtonElement>("#save-start").disabled = false;
});
$("#save-back").addEventListener("click", () => show("w-results"));

$("#save-start").addEventListener("click", async () => {
  const rows = await results.selectedRows();
  $("#save-progress").hidden = false;
  $("#save-status").textContent = `Writing ${rows.length.toLocaleString()} files to ${state.folder}…`;
  $<HTMLButtonElement>("#save-start").disabled = true;
  try {
    const written = await call<Written[]>("restore", {
      ids: rows.map((r) => r.id),
      out: state.folder,
      reassemble: $<HTMLInputElement>("#reassemble").checked,
    });
    const short = written.filter((w) => w.notes.some((n) => n.includes("short"))).length;
    $("#done-title").textContent = `${written.length.toLocaleString()} file${written.length === 1 ? "" : "s"} recovered`;
    $("#done-sub").textContent = `Saved in ${state.folder}`;
    $("#done-detail").innerHTML = `
      <dt>From</dt><dd>${escapeHtml(state.sourceLabel || state.source)}</dd>
      <dt>Asked for</dt><dd>${rows.length.toLocaleString()}</dd>
      <dt>Written</dt><dd>${written.length.toLocaleString()}</dd>
      ${short ? `<dt>Incomplete</dt><dd>${short} file(s) had parts that could not be read</dd>` : ""}`;
    show("w-done");
  } catch (e) {
    $("#save-status").innerHTML = `<span style="color:var(--bad)">${escapeHtml(String(e))}</span>`;
    $<HTMLButtonElement>("#save-start").disabled = false;
  } finally {
    $("#save-progress").hidden = true;
  }
});

$("#open-folder").addEventListener("click", () => call("open_folder", { path: state.folder }));
$("#done-more").addEventListener("click", () => show("w-what"));

// ---------------------------------------------------------------------------
// phone
// ---------------------------------------------------------------------------

async function runCli(args: string[], out: HTMLElement) {
  out.hidden = false;
  out.textContent = `rc ${args.join(" ")}\n…`;
  try {
    const r = await call<CliOutput>("run_cli", { args });
    let body = r.stdout;
    try {
      body = JSON.stringify(JSON.parse(r.stdout), null, 2);
    } catch {
      /* not JSON */
    }
    out.textContent = `${body || r.stderr || "(no output)"}`;
  } catch (e) {
    out.textContent = String(e);
  }
}

$("#phone-check").addEventListener("click", () => runCli(["android", "checklist"], $("#phone-out")));
$("#phone-pull").addEventListener("click", async () => {
  const folder = await pickFolder();
  if (folder) runCli(["android", "pull", "--out", folder], $("#phone-out"));
});
$("#ios-find").addEventListener("click", () => runCli(["ios", "backups"], $("#ios-out")));
$("#ios-trashed").addEventListener("click", async () => {
  const folder = await pickFolder();
  if (folder) runCli(["ios", "trashed", folder], $("#ios-out"));
});
$("#phone-back").addEventListener("click", () => show("w-what"));

// ---------------------------------------------------------------------------
// advanced view
// ---------------------------------------------------------------------------

$("#to-advanced").addEventListener("click", () => {
  show("adv");
  advResults.relayout();
});
$("#to-wizard").addEventListener("click", () => show(state.source ? "w-results" : "w-what"));
all<HTMLButtonElement>(".tabs [data-panel]").forEach((b) =>
  b.addEventListener("click", () => {
    all(".tabs [data-panel]").forEach((x) => x.setAttribute("aria-selected", String(x === b)));
    all<HTMLElement>("#adv .panel").forEach((p) => (p.hidden = p.id !== b.dataset.panel));
    if (b.dataset.panel === "p-results") advResults.relayout();
  }),
);

const advResults = new ResultsView($("#adv-results"), {
  lineHeight: 34,
  perLine: 1,
  render: (row, _i, selected) => {
    if (!row) return `<div class="vrow"><span class="name muted">…</span></div>`;
    const h = health(row);
    return `<div class="vrow ${selected ? "selected" : ""}" style="height:100%">
      <span class="badge ${h.cls}">${row.band || row.status}</span>
      <span class="name">${escapeHtml(row.name)}</span>
      <span class="where">${escapeHtml(row.path)}</span>
      <span class="size">${bytes(row.size)}</span>
      <span class="size">${escapeHtml(row.kind)}</span>
    </div>`;
  },
  emptyHtml: `${icon("empty")}<b>No rows</b>`,
  onSelectionChange: (n) => {
    $("#adv-selection").textContent = n ? `${n.toLocaleString()} selected` : "Nothing selected";
    $<HTMLButtonElement>("#adv-recover").disabled = n === 0;
  },
});



async function applyAdvancedFilter() {
  const checked = (name: string) => all<HTMLInputElement>(`input[name=${name}]:checked`).map((i) => i.value);
  const filter: Filter = {
    text: $<HTMLInputElement>("#adv-search").value.trim(),
    bands: checked("band"),
    kinds: checked("kind"),
    exts: $<HTMLInputElement>("#adv-ext")
      .value.split(/[\s,]+/)
      .map((s) => s.trim().toLowerCase().replace(/^\./, ""))
      .filter(Boolean),
    min_size: null,
    sort: null,
    descending: false,
  };
  $("#adv-count").textContent = "Filtering…";
  const t0 = performance.now();
  const n = await call<number>("set_view", { filter });
  advResults.setCount(n);
  $("#adv-count").textContent = `${n.toLocaleString()} rows · ${((performance.now() - t0) / 1000).toFixed(1)} s`;
}

let advTimer = 0;
all<HTMLInputElement>("#p-results input").forEach((i) =>
  i.addEventListener(i.type === "checkbox" ? "change" : "input", () => {
    clearTimeout(advTimer);
    advTimer = window.setTimeout(applyAdvancedFilter, 250);
  }),
);

$("#adv-pick").addEventListener("click", async () => {
  const p = await pickImageFile();
  if (p) $<HTMLInputElement>("#adv-source").value = p;
});
$("#adv-fs").addEventListener("click", async () => {
  $<HTMLButtonElement>("#adv-stop").disabled = false;
  await call("scan_filesystems", { source: $<HTMLInputElement>("#adv-source").value.trim() }).catch(
    (e) => ($("#adv-status").textContent = String(e)),
  );
});
$("#adv-carve").addEventListener("click", async () => {
  $<HTMLButtonElement>("#adv-stop").disabled = false;
  await call("carve", { source: $<HTMLInputElement>("#adv-source").value.trim() }).catch(
    (e) => ($("#adv-status").textContent = String(e)),
  );
});
$("#adv-stop").addEventListener("click", () => call("stop"));
$("#synthetic").addEventListener("click", async () => {
  const count = Number($<HTMLInputElement>("#synthetic-n").value);
  $("#adv-status").textContent = `Adding ${count.toLocaleString()} synthetic rows…`;
  const t0 = performance.now();
  await call("synthetic", { count });
  await applyAdvancedFilter();
  $("#adv-status").textContent = `Added in ${((performance.now() - t0) / 1000).toFixed(1)} s`;
});
$("#clear").addEventListener("click", async () => {
  await call("clear");
  await applyAdvancedFilter();
});
$("#adv-recover").addEventListener("click", async () => {
  const folder = await pickFolder();
  if (!folder) return;
  const rows = await advResults.selectedRows();
  $("#adv-selection").textContent = `Writing ${rows.length} files…`;
  try {
    const written = await call<Written[]>("restore", { ids: rows.map((r) => r.id), out: folder, reassemble: true });
    $("#adv-selection").textContent = `Wrote ${written.length} files to ${folder}`;
  } catch (e) {
    $("#adv-selection").textContent = String(e);
  }
});

// The command-line forms.
const TOOLS: { cmd: string; title: string; fields: { name: string; placeholder: string; required?: boolean; check?: boolean }[] }[] = [
  { cmd: "android checklist", title: "Android: is the phone ready?", fields: [{ name: "--serial", placeholder: "Serial (if several phones)" }] },
  { cmd: "android survey", title: "Android: list trashed media and caches", fields: [{ name: "--serial", placeholder: "Serial" }, { name: "--confirm-unlocked", placeholder: "The phone is unlocked", check: true }] },
  { cmd: "android pull", title: "Android: copy files to this computer", fields: [{ name: "--out", placeholder: "New folder", required: true }, { name: "--confirm-unlocked", placeholder: "The phone is unlocked", check: true }] },
  { cmd: "ios backups", title: "iPhone: backups on this computer", fields: [] },
  { cmd: "ios trashed", title: "iPhone: Recently Deleted in a backup", fields: [{ name: "", placeholder: "Backup folder", required: true }] },
  { cmd: "ios extract", title: "iPhone: extract Recently Deleted", fields: [{ name: "", placeholder: "Backup folder", required: true }, { name: "--out", placeholder: "New folder", required: true }] },
  { cmd: "host-backups", title: "Phone backups and sync caches on this PC", fields: [] },
  { cmd: "sqlite", title: "SQLite: recover deleted rows", fields: [{ name: "", placeholder: "Database file", required: true }, { name: "--table", placeholder: "Table (optional)" }] },
  { cmd: "image", title: "Image a failing drive", fields: [{ name: "", placeholder: "Device", required: true }, { name: "--output", placeholder: "Output .img on another disk", required: true }] },
  { cmd: "verify", title: "Verify an image against its hashes", fields: [{ name: "", placeholder: "Image file", required: true }] },
  { cmd: "bridge", title: "Receive files from the phone app over USB", fields: [{ name: "--out", placeholder: "New folder", required: true }, { name: "--port", placeholder: "Port (default 38300)" }] },
  { cmd: "list-deleted", title: "List deleted entries in full detail", fields: [{ name: "", placeholder: "Device or image", required: true }] },
];

$("#tools").innerHTML = TOOLS.map(
  (t, i) => `<form class="card tool" data-i="${i}">
    <h3>${escapeHtml(t.title)}</h3>
    ${t.fields
      .map((f) =>
        f.check
          ? `<label class="check"><input type="checkbox" data-arg="${f.name}" /> ${escapeHtml(f.placeholder)}</label>`
          : `<input type="text" data-arg="${f.name}" placeholder="${escapeHtml(f.placeholder)}" ${f.required ? "required" : ""} />`,
      )
      .join("")}
    <div class="inline"><button class="primary">Run</button><span class="faint mono">rc ${escapeHtml(t.cmd)}</span></div>
    <pre class="out" hidden></pre>
  </form>`,
).join("");

all<HTMLFormElement>("#tools form").forEach((form) =>
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const tool = TOOLS[Number(form.dataset.i)];
    const args = tool.cmd.split(" ");
    form.querySelectorAll<HTMLInputElement>("[data-arg]").forEach((input) => {
      const arg = input.dataset.arg!;
      if (input.type === "checkbox") {
        if (input.checked) args.push(arg);
      } else if (input.value.trim()) {
        if (arg) args.push(arg, input.value.trim());
        else args.push(input.value.trim());
      }
    });
    runCli(args, form.querySelector("pre")!);
  }),
);

// ---------------------------------------------------------------------------

// The first command that answers settles which backend is there; the banner
// follows it, so the app can never quietly show made-up rows as real ones.
onBackendKnown((real) => {
  $("#browser-note").hidden = real;
  if (!real) {
    $("#browser-note").innerHTML =
      `<svg class="i"><use href="#i-warn" /></svg><div><b>Interface preview.</b> The engine did not ` +
      `answer, so every file listed here is made up. <span class="faint mono">${escapeHtml(bridgeError)}</span></div>`;
  }
  $("#mode-note").textContent = real
    ? "Nothing is ever written to the drive you are recovering from."
    : "Interface preview — no engine behind it.";
});
// Something to settle it before the user touches anything. If the engine is
// not there, say why on screen rather than only in a console nobody opens.
call<boolean>("elevation").catch(() => {});
show("w-what");
(window as any).rcResults = results;

