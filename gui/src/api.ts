// The only way the interface reaches the engine.
//
// Inside the desktop app these call the Rust commands. Opened in a plain
// browser — for development, and for measuring the results view at ten million
// rows — they fall back to a stand-in that makes rows up, so every screen can
// be exercised without a device. `inTauri` decides, from the presence of
// Tauri's bridge, and the interface shows a banner when it is the stand-in.

export interface Row {
  id: number;
  kind: string;
  source: string;
  name: string;
  path: string;
  ext: string;
  size: number;
  band: string;
  score: number;
  status: string;
  offset: number | null;
  reasons: string;
  modified: number | null;
}

export interface Filter {
  text: string;
  /** Only files that used to be inside this folder (relative to the drive). */
  folder?: string;
  bands: string[];
  kinds: string[];
  exts: string[];
  min_size: number | null;
  sort: string | null;
  descending: boolean;
}

export interface Progress {
  phase: string;
  done: number;
  total: number;
  found: number;
  /** Whether done/total count bytes of the drive or files. */
  unit?: "bytes" | "items";
}

export interface Device {
  path: string;
  kind: string;
  model: string | null;
  size_bytes: number;
  readable: boolean;
  note: string | null;
  removable: boolean | null;
  rotational: boolean | null;
}

export interface Volume {
  letter: string;
  device_path: string;
  mount: string;
  label: string | null;
  filesystem: string | null;
  total_bytes: number;
  free_bytes: number;
  removable: boolean;
  readable: boolean;
  note: string | null;
}

export interface Written {
  source: string;
  dest: string;
  bytes: number;
  sha256: string;
  notes: string[];
}

export interface CliOutput {
  code: number | null;
  stdout: string;
  stderr: string;
  program: string;
}

/**
 * Which backend is behind the interface.
 *
 * Not decided by looking for a global Tauri may or may not inject: the first
 * command that actually answers settles it. Until then calls are tried against
 * the engine and fall back to the stand-in, so there is no boot race and the
 * banner can never claim the wrong thing.
 */
type Mode = "unknown" | "engine" | "stand-in";
let mode: Mode = "unknown";
let announce: (real: boolean) => void = () => {};
/** Why the engine was not reachable, if it was not. */
export let bridgeError = "";

export const inTauri = () => mode === "engine";

/** Called once, when the first command settles which backend is there. */
export function onBackendKnown(fn: (real: boolean) => void) {
  announce = fn;
  if (mode !== "unknown") fn(mode === "engine");
}

function settle(m: Exclude<Mode, "unknown">) {
  if (mode === m) return;
  mode = m;
  announce(m === "engine");
}

type Handler<T> = (payload: T) => void;
const mockListeners: Record<string, Handler<any>[]> = {};

export async function listen<T>(event: string, handler: Handler<T>): Promise<void> {
  if (mode !== "stand-in") {
    try {
      const ev = await import("@tauri-apps/api/event");
      await ev.listen<T>(event, (e) => handler(e.payload));
      return;
    } catch {
      /* no bridge: fall through to the stand-in */
    }
  }
  (mockListeners[event] ??= []).push(handler);
}

export async function call<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  if (mode === "stand-in") return mock<T>(cmd, args);
  try {
    const core = await import("@tauri-apps/api/core");
    const out = await core.invoke<T>(cmd, args);
    settle("engine");
    return out;
  } catch (e) {
    // Once the engine has answered once, a later error is the engine's answer
    // and belongs to the caller - not a reason to invent data.
    if (mode === "engine") throw e;
    bridgeError = `${cmd}: ${e instanceof Error ? e.message : String(e)}`;
    settle("stand-in");
    return mock<T>(cmd, args);
  }
}

/** The system folder chooser. */
export async function pickFolder(title = "Save recovered files in"): Promise<string | null> {
  if (inTauri()) {
    const dialog = await import("@tauri-apps/plugin-dialog");
    const chosen = await dialog.open({ directory: true, multiple: false, title });
    return typeof chosen === "string" ? chosen : null;
  }
  return window.prompt("Folder to save into (stand-in backend)", "D:\\recovered");
}

export async function pickImageFile(): Promise<string | null> {
  if (inTauri()) {
    const dialog = await import("@tauri-apps/plugin-dialog");
    const chosen = await dialog.open({
      multiple: false,
      title: "Open a disk image",
      filters: [{ name: "Disk images", extensions: ["img", "raw", "dd", "bin", "iso"] }],
    });
    return typeof chosen === "string" ? chosen : null;
  }
  return window.prompt("Disk image path (stand-in backend)", "D:\\images\\card.img");
}

// ---------------------------------------------------------------------------
// stand-in backend, browser only
// ---------------------------------------------------------------------------

const EXTS = ["jpg", "png", "heic", "mp4", "mov", "pdf", "docx", "xlsx", "txt", "mp3", "zip", "sqlite"];
const FOLDERS = ["DCIM/Camera", "Pictures/Holiday", "Documents/Work", "Downloads", "Music", "Videos/2024"];
let mockRows = 0;
let mockView: { len: number; map: (i: number) => number } = { len: 0, map: (i) => i + 1 };
let mockFilter: Filter | null = null;

function mockRow(id: number): Row {
  const n = Number((BigInt(id) * 2654435761n) % 100000n);
  const ext = EXTS[n % EXTS.length];
  const score = n % 101;
  const folder = FOLDERS[(n >> 3) % FOLDERS.length];
  const name = `${ext === "jpg" || ext === "png" ? "IMG" : "file"}_${String(id).padStart(5, "0")}.${ext}`;
  return {
    id,
    kind: n % 3 === 0 ? "carved" : "deleted",
    source: "\\\\.\\PhysicalDrive2",
    name,
    path: `${folder}/${name}`,
    ext,
    size: 20_000 + ((n * 7919) % 8_000_000),
    band: score >= 90 ? "GREEN" : score >= 45 ? "YELLOW" : "RED",
    score,
    status: n % 3 === 0 ? (score > 60 ? "valid" : "partial") : "Traversed",
    offset: id * 4096,
    reasons: score >= 90 ? "" : "clusters-lost: 2 of 14 clusters are not readable as this file",
    modified: 1_700_000_000 + ((n * 37) % 40_000_000),
  };
}

function matches(r: Row, f: Filter): boolean {
  if (f.text && !r.path.toLowerCase().includes(f.text.toLowerCase())) return false;
  if (f.bands.length && !f.bands.includes(r.band)) return false;
  if (f.kinds.length && !f.kinds.includes(r.kind)) return false;
  if (f.exts.length && !f.exts.includes(r.ext)) return false;
  return true;
}

function mockThumb(row: Row): string {
  const hue = (row.id * 47) % 360;
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="220" height="160">
    <rect width="220" height="160" fill="hsl(${hue} 45% 42%)"/>
    <circle cx="60" cy="52" r="20" fill="hsl(${(hue + 40) % 360} 70% 72%)"/>
    <path d="M0 160 L80 84 L140 130 L180 104 L220 140 L220 160 Z" fill="hsl(${(hue + 200) % 360} 40% 26%)"/>
  </svg>`;
  return `data:image/svg+xml;base64,${btoa(svg)}`;
}

async function mock<T>(cmd: string, a: any): Promise<T> {
  await new Promise((r) => setTimeout(r, 1));
  switch (cmd) {
    case "elevation":
      return false as T;
    case "startup_request":
      return null as T;
    case "check_source": {
      // A raw drive needs Administrator on Windows; the stand-in says so too,
      // so the interface's answer to that can be seen without elevation.
      const source = String(a.source);
      const raw = /^\\\\[.?]\\[A-Za-z]:$/.test(source) || source.includes("PhysicalDrive");
      return {
        ok: !raw,
        needs_admin: raw,
        message: raw
          ? `Windows only lets a program read ${source} with Administrator rights.`
          : "",
      } as T;
    }
    case "volumes":
      return [
        { letter: "C", device_path: "\\\\.\\C:", mount: "C:\\", label: null, filesystem: "NTFS", total_bytes: 476 * 2 ** 30, free_bytes: 88 * 2 ** 30, removable: false, readable: false, note: "requires Administrator to read sector data" },
        { letter: "D", device_path: "\\\\.\\D:", mount: "D:\\", label: "Data", filesystem: "NTFS", total_bytes: 477 * 2 ** 30, free_bytes: 237 * 2 ** 30, removable: false, readable: true, note: null },
        { letter: "E", device_path: "\\\\.\\E:", mount: "E:\\", label: "SANDISK", filesystem: "exFAT", total_bytes: 64 * 2 ** 30, free_bytes: 51 * 2 ** 30, removable: true, readable: true, note: null },
      ] as T;
    case "devices":
      return [
        { path: "\\\\.\\PhysicalDrive2", kind: "Removable", model: "SanDisk Ultra USB", size_bytes: 64 * 2 ** 30, readable: true, note: null, removable: true, rotational: false },
        { path: "\\\\.\\PhysicalDrive1", kind: "Fixed", model: "Samsung SSD 990 PRO", size_bytes: 2 * 2 ** 40, readable: true, note: null, removable: false, rotational: false },
        { path: "\\\\.\\PhysicalDrive0", kind: "Fixed", model: "System disk", size_bytes: 512 * 2 ** 30, readable: false, note: "requires Administrator", removable: false, rotational: false },
      ] as T;
    case "start_recovery": {
      mockRows = 0;
      let done = 0;
      const total = 64 * 2 ** 30;
      const tick = setInterval(() => {
        done += total / 22;
        mockRows = Math.min(4000, mockRows + 320);
        (mockListeners["progress"] ?? []).forEach((h) =>
          h({
            phase: done < total / 2 ? "reading the list of files on this drive" : "searching every sector of the drive",
            done,
            total,
            found: mockRows,
            unit: done < total / 2 ? "items" : "bytes",
          }),
        );
        if (done >= total) {
          clearInterval(tick);
          mockView = { len: mockRows, map: (i) => i + 1 };
          (mockListeners["scan-done"] ?? []).forEach((h) =>
            h({ ok: true, summary: { rows_added: mockRows, notes: ["this is the stand-in backend: the files are made up"] } }),
          );
        }
      }, 180);
      return undefined as T;
    }
    case "stop":
      return undefined as T;
    case "synthetic":
      mockRows += a.count;
      mockView = { len: mockRows, map: (i) => i + 1 };
      return mockRows as T;
    case "view_len":
      return mockView.len as T;
    case "set_view": {
      const f: Filter = a.filter;
      mockFilter = f;
      const identity = !f.text && !f.bands.length && !f.kinds.length && !f.exts.length;
      if (identity) {
        mockView = { len: mockRows, map: (i) => i + 1 };
      } else if (mockRows > 200_000) {
        // Too many to filter one by one in a stand-in: keep every third.
        mockView = { len: Math.floor(mockRows / 3), map: (i) => i * 3 + 1 };
      } else {
        const ids: number[] = [];
        for (let i = 1; i <= mockRows; i++) if (matches(mockRow(i), f)) ids.push(i);
        mockView = { len: ids.length, map: (i) => ids[i] };
      }
      return mockView.len as T;
    }
    case "rows": {
      const out: Row[] = [];
      for (let i = a.start; i < Math.min(a.start + a.count, mockView.len); i++) out.push(mockRow(mockView.map(i)));
      return out as T;
    }
    case "phone_screen":
      throw new Error("the desktop app talks to the phone; this is the interface preview");
    case "thumbnail": {
      const r = mockRow(a.id);
      return (["jpg", "png", "heic", "mp4", "mov"].includes(r.ext) ? mockThumb(r) : null) as T;
    }
    case "preview":
      return mockThumb(mockRow(a.id)) as T;
    case "restore":
      return [...Array(Math.min(a.ids.length, 500))].map((_, i) => ({
        source: mockRow(a.ids[i] ?? 1).path,
        dest: `${a.out}\\${mockRow(a.ids[i] ?? 1).name}`,
        bytes: 1234,
        sha256: "0".repeat(64),
        notes: [],
      })) as T;
    case "clear":
      mockRows = 0;
      mockView = { len: 0, map: (i) => i + 1 };
      return undefined as T;
    case "open_folder":
      window.alert("The desktop app would open this folder now.");
      return undefined as T;
    default:
      throw new Error(`"${cmd}" needs the desktop app — this is the browser preview.`);
  }
}

export function mockFilterState() {
  return mockFilter;
}
