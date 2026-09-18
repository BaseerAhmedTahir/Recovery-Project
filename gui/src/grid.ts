// The results view: a virtual list that renders only what is on screen, in
// either of two shapes — a list of rows, or a grid of picture tiles.
//
// Browsers cap an element's height near 33 million pixels, and 10 million rows
// at 32 px is 320 million, so the scrollable spacer is capped and the scroll
// position is scaled: the virtualizer is told the offset in row space
// (scrollTop x scale) and items are placed back into pixel space relative to
// the real scrollTop. At that size one pixel of scrollbar is several rows, and
// the keyboard moves one row at a time.

import { Virtualizer, observeElementRect } from "@tanstack/virtual-core";
import { call, Row } from "./api";

const MAX_PX = 8_000_000;
const PAGE = 100;

export interface ViewOptions {
  /** How tall one line of items is, in pixels. */
  lineHeight: number;
  /** Items per line: 1 for a list, more for a tile grid. */
  perLine: number;
  /** Markup for one item. `index` is its position in the view. */
  render: (row: Row | undefined, index: number, selected: boolean) => string;
  onSelectionChange?: (count: number) => void;
  onOpen?: (row: Row) => void;
  emptyHtml?: string;
}

export class ResultsView {
  private scroller: HTMLElement;
  private spacer: HTMLElement;
  private body: HTMLElement;
  private emptyEl: HTMLElement;
  private count = 0;
  private scale = 1;
  private cache = new Map<number, Row>();
  private pending = new Set<number>();
  private generation = 0;
  private v: Virtualizer<HTMLElement, HTMLElement>;
  private opts: ViewOptions;
  readonly selected = new Set<number>();
  private anchor: number | null = null;
  private focused = 0;
  /** Lines rendered in the last frame; the responsiveness measurement reads it. */
  lastRendered = 0;

  constructor(host: HTMLElement, opts: ViewOptions) {
    this.opts = opts;
    host.innerHTML = "";
    this.scroller = el("div", "scroller");
    this.scroller.tabIndex = 0;
    this.spacer = el("div", "spacer");
    this.body = el("div", "vbody");
    this.emptyEl = el("div", "empty");
    this.emptyEl.innerHTML = opts.emptyHtml ?? "";
    this.spacer.appendChild(this.body);
    this.scroller.appendChild(this.spacer);
    host.appendChild(this.scroller);
    host.appendChild(this.emptyEl);

    this.v = new Virtualizer<HTMLElement, HTMLElement>({
      count: 0,
      getScrollElement: () => this.scroller,
      estimateSize: () => this.opts.lineHeight,
      overscan: 4,
      observeElementRect,
      observeElementOffset: (instance, cb) => {
        const node = instance.scrollElement;
        if (!node) return;
        const handler = () => {
          this.rescale();
          cb(node.scrollTop * this.scale, true);
        };
        handler();
        node.addEventListener("scroll", handler, { passive: true });
        return () => node.removeEventListener("scroll", handler);
      },
      scrollToFn: (offset, o, instance) => {
        instance.scrollElement?.scrollTo({ top: offset / this.scale, behavior: o.behavior });
      },
      onChange: () => this.render(),
    });
    this.v._didMount();
    this.v._willUpdate();

    this.body.addEventListener("click", (e) => {
      const item = (e.target as HTMLElement).closest("[data-index]") as HTMLElement | null;
      if (!item) return;
      this.click(Number(item.dataset.index), e);
    });
    this.body.addEventListener("dblclick", (e) => {
      const item = (e.target as HTMLElement).closest("[data-index]") as HTMLElement | null;
      const row = item && this.cache.get(Number(item.dataset.index));
      if (row) this.opts.onOpen?.(row);
    });
    this.scroller.addEventListener("keydown", (e) => this.key(e));
    window.addEventListener("resize", () => this.relayout());
  }

  private get lines() {
    return Math.ceil(this.count / this.opts.perLine);
  }

  setCount(count: number) {
    this.generation++;
    this.count = count;
    this.cache.clear();
    this.pending.clear();
    this.selected.clear();
    this.anchor = null;
    this.focused = 0;
    this.emptyEl.hidden = count > 0;
    this.relayout();
    this.scroller.scrollTop = 0;
    this.opts.onSelectionChange?.(0);
  }

  /** Re-measure after the shape or the window changed. */
  relayout() {
    this.spacer.style.height = `${Math.min(this.lines * this.opts.lineHeight, MAX_PX)}px`;
    this.rescale();
    this.v.setOptions({ ...this.v.options, count: this.lines, estimateSize: () => this.opts.lineHeight });
    this.v._willUpdate();
    this.v.measure();
    this.render();
  }

  setShape(lineHeight: number, perLine: number, render: ViewOptions["render"]) {
    this.opts = { ...this.opts, lineHeight, perLine, render };
    this.relayout();
  }

  private rescale() {
    const total = this.lines * this.opts.lineHeight;
    const capped = Math.min(total, MAX_PX);
    const vh = this.scroller.clientHeight;
    this.scale =
      total > capped ? (vh > 0 ? (total - vh) / Math.max(1, capped - vh) : total / capped) : 1;
  }

  private fetch(firstLine: number, lastLine: number) {
    const first = firstLine * this.opts.perLine;
    const last = Math.min(this.count - 1, (lastLine + 1) * this.opts.perLine - 1);
    const gen = this.generation;
    for (let p = Math.floor(first / PAGE); p <= Math.floor(last / PAGE); p++) {
      if (this.pending.has(p)) continue;
      const start = p * PAGE;
      const end = Math.min(start + PAGE, this.count) - 1;
      if (this.cache.has(start) && this.cache.has(end)) continue;
      this.pending.add(p);
      call<Row[]>("rows", { start, count: PAGE })
        .then((rows) => {
          if (gen !== this.generation) return;
          rows.forEach((r, i) => this.cache.set(start + i, r));
          if (this.cache.size > 20_000) {
            [...this.cache.keys()].slice(0, this.cache.size - 10_000).forEach((k) => this.cache.delete(k));
          }
          this.render();
        })
        .catch(() => {})
        .finally(() => this.pending.delete(p));
    }
  }

  private render() {
    this.rescale();
    const items = this.v.getVirtualItems();
    if (items.length) this.fetch(items[0].index, items[items.length - 1].index);
    const shift = this.scroller.scrollTop * (this.scale - 1);
    let html = "";
    for (const line of items) {
      const top = line.start - shift;
      for (let c = 0; c < this.opts.perLine; c++) {
        const index = line.index * this.opts.perLine + c;
        if (index >= this.count) break;
        html += `<div class="slot" style="position:absolute;top:${top}px;height:${this.opts.lineHeight}px;${
          this.opts.perLine > 1
            ? `left:calc(${(100 / this.opts.perLine).toFixed(4)}% * ${c});width:calc(${(
                100 / this.opts.perLine
              ).toFixed(4)}% );padding:6px;`
            : "left:0;right:0;"
        }" data-index="${index}">${this.opts.render(
          this.cache.get(index),
          index,
          this.selected.has(index),
        )}</div>`;
      }
    }
    this.body.innerHTML = html;
    this.lastRendered = items.length;
  }

  private click(index: number, e: MouseEvent) {
    if (e.shiftKey && this.anchor !== null) {
      const [a, b] = [Math.min(this.anchor, index), Math.max(this.anchor, index)];
      if (b - a <= 200_000) {
        if (!(e.ctrlKey || e.metaKey)) this.selected.clear();
        for (let i = a; i <= b; i++) this.selected.add(i);
      }
    } else if (e.ctrlKey || e.metaKey) {
      this.selected.has(index) ? this.selected.delete(index) : this.selected.add(index);
      this.anchor = index;
    } else {
      // A plain click toggles: this is a picking list, not a document.
      this.selected.has(index) ? this.selected.delete(index) : this.selected.add(index);
      this.anchor = index;
    }
    this.focused = index;
    this.render();
    this.opts.onSelectionChange?.(this.selected.size);
  }

  private key(e: KeyboardEvent) {
    const perScreen = Math.max(1, Math.floor(this.scroller.clientHeight / this.opts.lineHeight)) * this.opts.perLine;
    const moves: Record<string, number> = {
      ArrowDown: this.opts.perLine,
      ArrowUp: -this.opts.perLine,
      ArrowRight: 1,
      ArrowLeft: -1,
      PageDown: perScreen,
      PageUp: -perScreen,
      Home: -Infinity,
      End: Infinity,
    };
    if (e.key === " " || e.key === "Enter") {
      e.preventDefault();
      this.selected.has(this.focused) ? this.selected.delete(this.focused) : this.selected.add(this.focused);
      this.render();
      this.opts.onSelectionChange?.(this.selected.size);
      return;
    }
    if (!(e.key in moves) || this.count === 0) return;
    e.preventDefault();
    this.focused = Math.max(0, Math.min(this.count - 1, this.focused + moves[e.key]));
    this.v.scrollToIndex(Math.floor(this.focused / this.opts.perLine), { align: "auto" });
    this.render();
  }

  selectAll() {
    for (let i = 0; i < this.count; i++) this.selected.add(i);
    this.render();
    this.opts.onSelectionChange?.(this.selected.size);
  }

  clearSelection() {
    this.selected.clear();
    this.render();
    this.opts.onSelectionChange?.(0);
  }

  /** The rows behind the current selection, fetched if they are not cached. */
  async selectedRows(limit = 20_000): Promise<Row[]> {
    const wanted = [...this.selected].slice(0, limit).sort((a, b) => a - b);
    const missing = wanted.filter((i) => !this.cache.has(i));
    for (let k = 0; k < missing.length; k += PAGE) {
      const start = missing[k];
      const rows = await call<Row[]>("rows", { start, count: PAGE });
      rows.forEach((r, i) => this.cache.set(start + i, r));
    }
    return wanted.map((i) => this.cache.get(i)).filter((r): r is Row => !!r);
  }

  row(index: number): Row | undefined {
    return this.cache.get(index);
  }

  get length() {
    return this.count;
  }

  /** Used by the responsiveness measurement. */
  scrollToFraction(f: number) {
    this.scroller.scrollTop = f * (this.scroller.scrollHeight - this.scroller.clientHeight);
  }

  /** Re-render in place, for when a thumbnail arrives. */
  refresh() {
    this.render();
  }
}

function el(tag: string, cls: string): HTMLElement {
  const node = document.createElement(tag);
  node.className = cls;
  return node;
}

export function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}
