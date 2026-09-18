// The results grid: TanStack Virtual (virtual-core) over a row source that is
// fetched a window at a time.
//
// Browsers cap an element's height (about 33 million px in Chromium), and 10
// million rows at 28 px is 280 million. So the scrollable spacer is capped and
// the scroll position is scaled: the virtualizer is told the offset in row
// space (scrollTop x scale), and rows are placed back in pixel space relative
// to the real scrollTop. At 10 million rows one pixel of scrollbar is about
// eight rows of data; the keyboard moves row by row.

import { Virtualizer, observeElementRect } from "@tanstack/virtual-core";
import { call, Row } from "./api";

export const ROW_H = 28;
const MAX_PX = 8_000_000;
const PAGE = 100;

export interface Column {
  key: keyof Row | "sel";
  label: string;
  width: string;
  sortable?: boolean;
  render?: (r: Row) => string;
}

export class Grid {
  private scroller: HTMLElement;
  private spacer: HTMLElement;
  private body: HTMLElement;
  private count = 0;
  private scale = 1;
  private cache = new Map<number, Row>();
  private pending = new Set<number>();
  private generation = 0;
  private v: Virtualizer<HTMLElement, HTMLElement>;
  selected = new Set<number>();
  anchor: number | null = null;
  focusIndex = 0;
  onSelect: (rows: Row[]) => void = () => {};
  /** Rows rendered in the last frame, for the responsiveness measurement. */
  lastRendered = 0;

  constructor(host: HTMLElement, private columns: Column[]) {
    host.innerHTML = "";
    this.scroller = document.createElement("div");
    this.scroller.className = "grid-scroll";
    this.scroller.tabIndex = 0;
    this.spacer = document.createElement("div");
    this.spacer.className = "grid-spacer";
    this.body = document.createElement("div");
    this.body.className = "grid-body";
    this.spacer.appendChild(this.body);
    this.scroller.appendChild(this.spacer);
    host.appendChild(this.scroller);

    this.v = new Virtualizer<HTMLElement, HTMLElement>({
      count: 0,
      getScrollElement: () => this.scroller,
      estimateSize: () => ROW_H,
      overscan: 8,
      observeElementRect,
      observeElementOffset: (instance, cb) => {
        const el = instance.scrollElement;
        if (!el) return;
        const handler = () => {
          this.rescale();
          cb(el.scrollTop * this.scale, true);
        };
        handler();
        el.addEventListener("scroll", handler, { passive: true });
        return () => el.removeEventListener("scroll", handler);
      },
      scrollToFn: (offset, opts, instance) => {
        instance.scrollElement?.scrollTo({ top: offset / this.scale, behavior: opts.behavior });
      },
      onChange: () => this.render(),
    });
    this.v._didMount();
    this.v._willUpdate();

    this.scroller.addEventListener("keydown", (e) => this.key(e));
    this.body.addEventListener("click", (e) => {
      const rowEl = (e.target as HTMLElement).closest(".grid-row") as HTMLElement | null;
      if (!rowEl) return;
      this.click(Number(rowEl.dataset.index), e);
    });
  }

  setCount(count: number) {
    this.generation++;
    this.count = count;
    this.cache.clear();
    this.pending.clear();
    this.selected.clear();
    this.anchor = null;
    this.focusIndex = 0;
    this.spacer.style.height = `${Math.min(count * ROW_H, MAX_PX)}px`;
    this.rescale();
    this.v.setOptions({ ...this.v.options, count });
    this.scroller.scrollTop = 0;
    this.v._willUpdate();
    this.v.measure();
    this.render();
    this.onSelect([]);
  }

  refresh() {
    this.generation++;
    this.cache.clear();
    this.pending.clear();
    this.render();
  }

  /** Scroll-pixel to row-space factor. Depends on the viewport height, which
   * is zero while the grid is hidden, so it is recomputed as it changes. */
  private rescale() {
    const total = this.count * ROW_H;
    const spacerPx = Math.min(total, MAX_PX);
    const vh = this.scroller.clientHeight;
    this.scale = total > spacerPx && vh > 0 ? (total - vh) / Math.max(1, spacerPx - vh) : total > spacerPx ? total / spacerPx : 1;
  }

  private fetch(first: number, last: number) {
    const gen = this.generation;
    for (let p = Math.floor(first / PAGE); p <= Math.floor(last / PAGE); p++) {
      if (this.pending.has(p)) continue;
      const start = p * PAGE;
      if (this.cache.has(start) && this.cache.has(Math.min(start + PAGE, this.count) - 1)) continue;
      this.pending.add(p);
      call<Row[]>("rows", { start, count: PAGE })
        .then((rows) => {
          if (gen !== this.generation) return;
          rows.forEach((r, i) => this.cache.set(start + i, r));
          if (this.cache.size > 20_000) {
            // Keep memory flat on a long scroll: drop the oldest pages.
            const keys = [...this.cache.keys()].slice(0, this.cache.size - 10_000);
            keys.forEach((k) => this.cache.delete(k));
          }
          this.render();
        })
        .finally(() => this.pending.delete(p));
    }
  }

  private render() {
    this.rescale();
    const items = this.v.getVirtualItems();
    const top = this.scroller.scrollTop;
    const shift = top * (this.scale - 1);
    if (items.length) this.fetch(items[0].index, items[items.length - 1].index);
    let html = "";
    for (const it of items) {
      const r = this.cache.get(it.index);
      const cls = `grid-row${this.selected.has(it.index) ? " selected" : ""}${
        it.index === this.focusIndex ? " focus" : ""
      }${r ? ` band-${r.band.toLowerCase()}` : " loading"}`;
      html += `<div class="${cls}" data-index="${it.index}" style="top:${it.start - shift}px">`;
      for (const c of this.columns) {
        let text = "";
        if (r) text = c.render ? c.render(r) : escapeHtml(String(r[c.key as keyof Row] ?? ""));
        else if (c.key === "name") text = "…";
        html += `<div class="cell" style="width:${c.width}">${text}</div>`;
      }
      html += "</div>";
    }
    this.body.innerHTML = html;
    this.lastRendered = items.length;
  }

  private click(index: number, e: MouseEvent) {
    if (e.shiftKey && this.anchor !== null) {
      const [a, b] = [Math.min(this.anchor, index), Math.max(this.anchor, index)];
      if (b - a > 100_000) return; // a selection this size belongs to a filter
      if (!e.ctrlKey) this.selected.clear();
      for (let i = a; i <= b; i++) this.selected.add(i);
    } else if (e.ctrlKey || e.metaKey) {
      if (this.selected.has(index)) this.selected.delete(index);
      else this.selected.add(index);
      this.anchor = index;
    } else {
      this.selected.clear();
      this.selected.add(index);
      this.anchor = index;
    }
    this.focusIndex = index;
    this.render();
    this.emitSelection();
  }

  private key(e: KeyboardEvent) {
    const page = Math.max(1, Math.floor(this.scroller.clientHeight / ROW_H) - 1);
    const moves: Record<string, number> = {
      ArrowDown: 1,
      ArrowUp: -1,
      PageDown: page,
      PageUp: -page,
      Home: -Infinity,
      End: Infinity,
    };
    if (!(e.key in moves) || this.count === 0) return;
    e.preventDefault();
    const next = Math.max(0, Math.min(this.count - 1, this.focusIndex + moves[e.key]));
    this.focusIndex = next;
    this.selected.clear();
    this.selected.add(next);
    this.anchor = next;
    this.v.scrollToIndex(next, { align: "auto" });
    this.render();
    this.emitSelection();
  }

  private async emitSelection() {
    const idx = [...this.selected].slice(0, 5000);
    const missing = idx.filter((i) => !this.cache.has(i));
    for (const i of missing) {
      const [r] = await call<Row[]>("rows", { start: i, count: 1 });
      if (r) this.cache.set(i, r);
    }
    this.onSelect(idx.map((i) => this.cache.get(i)).filter((r): r is Row => !!r));
  }

  get length() {
    return this.count;
  }

  /** For the measurement: scroll to a fraction of the way down. */
  scrollToFraction(f: number) {
    this.scroller.scrollTop = f * (this.scroller.scrollHeight - this.scroller.clientHeight);
  }
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}
