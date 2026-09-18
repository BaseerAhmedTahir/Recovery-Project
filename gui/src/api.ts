// The GUI's only way to reach the engine. Inside Tauri it invokes the Rust
// commands; opened in a plain browser (for development and the 10M-row
// frontend measurement) it uses a mock that generates rows on demand, so the
// page can be exercised without a device. The mock is never used in the app:
// `inTauri` is decided by the presence of Tauri's IPC bridge.

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

export interface HexView {
  offset: number;
  bytes: number[];
  spans: { start: number; end: number; kind: string; label: string }[];
}

export interface CliOutput {
  code: number | null;
  stdout: string;
  stderr: string;
  program: string;
}

export const inTauri = typeof (window as any).__TAURI_INTERNALS__ !== "undefined";

type Handler<T> = (payload: T) => void;

async function tauriInvoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const core = await import("@tauri-apps/api/core");
  return core.invoke<T>(cmd, args);
}

export async function listen<T>(event: string, handler: Handler<T>): Promise<() => void> {
  if (inTauri) {
    const ev = await import("@tauri-apps/api/event");
    return ev.listen<T>(event, (e) => handler(e.payload));
  }
  mockListeners[event] = [...(mockListeners[event] ?? []), handler as Handler<unknown>];
  return () => {};
}

// ---------------------------------------------------------------------------
// mock backend (browser only)
// ---------------------------------------------------------------------------

const mockListeners: Record<string, Handler<unknown>[]> = {};
let mockTotal = 0;
let mockView: { len: number; map: (i: number) => number } = { len: 0, map: (i) => i + 1 };
const EXTS = ["jpg", "png", "mp4", "pdf", "docx", "sqlite", "txt", "zip"];

function mockRow(id: number): Row {
  let x = BigInt(id) * 0x9e3779b97f4a7c15n;
  x = (x ^ (x >> 29n)) & 0xffffffffffffn;
  const n = Number(x);
  const score = n % 101;
  const ext = EXTS[n % 8];
  return {
    id,
    kind: "synthetic",
    source: "mock",
    name: `file_${String(id).padStart(8, "0")}.${ext}`,
    path: `dir_${String((n >> 8) % 5000).padStart(4, "0")}/file_${id}.${ext}`,
    ext,
    size: (n >> 12) % (64 << 20),
    band: score >= 90 ? "GREEN" : score >= 45 ? "YELLOW" : "RED",
    score,
    status: "synthetic",
    offset: id * 4096,
    reasons: "",
    modified: 1600000000 + ((n >> 5) % 100000000),
  };
}

async function mock<T>(cmd: string, a: any): Promise<T> {
  await new Promise((r) => setTimeout(r, 1));
  switch (cmd) {
    case "synthetic":
      mockTotal += a.count;
      mockView = { len: mockTotal, map: (i) => i + 1 };
      return mockTotal as T;
    case "view_len":
      return mockView.len as T;
    case "set_view": {
      const f: Filter = a.filter;
      if (!f.bands.length && !f.exts.length && !f.text) {
        mockView = { len: mockTotal, map: (i) => i + 1 };
      } else {
        // Every 3rd row, standing in for a filtered view.
        mockView = { len: Math.floor(mockTotal / 3), map: (i) => i * 3 + 1 };
      }
      return mockView.len as T;
    }
    case "rows": {
      const out: Row[] = [];
      for (let i = a.start; i < Math.min(a.start + a.count, mockView.len); i++) {
        out.push(mockRow(mockView.map(i)));
      }
      return out as T;
    }
    case "clear":
      mockTotal = 0;
      mockView = { len: 0, map: (i) => i + 1 };
      return undefined as T;
    case "devices":
      return [] as T;
    default:
      throw new Error(`${cmd} needs the desktop app (this is the browser preview)`);
  }
}

export function call<T>(cmd: string, args: Record<string, unknown> = {}): Promise<T> {
  return inTauri ? tauriInvoke<T>(cmd, args) : mock<T>(cmd, args);
}
