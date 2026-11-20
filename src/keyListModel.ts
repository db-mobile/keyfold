// State and actions of the navigation pane and of the flat list in it. The
// markup is in `KeyList.tsx`. The list caches at most a few dozen windows of
// 200 rows. Windows are only requested once scrolling has settled, so a
// fling never issues requests; metadata for visible rows follows 150 ms later.

import { createVirtualizer } from "@tanstack/solid-virtual";
import { batch, createEffect, createMemo, createSignal, on, onCleanup, onMount, type JSX } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { api, fmtBytes, fmtCount, fmtTtl, WINDOW, type Connection, type DeleteTarget, type KeyMeta, type KeyRef, type Row, type ScanProgress } from "./api";
import { keyId, sameKey, textKey } from "./keyRef";
import { LIST_ROW } from "./ui/density";

const ROW_HEIGHT = LIST_ROW;
const MAX_CACHED_WINDOWS = 32;
const SCROLL_SETTLE_MS = 80;
const META_DEBOUNCE_MS = 150;
const SCAN_REFRESH_MS = 2000;

type Windows = Record<number, Row[] | undefined>;

export type NavView = "tree" | "list";

export interface KeyListProps {
  view: NavView;
  onView: (view: NavView) => void;
  tree: JSX.Element;
  /// Scan status and controls, at the foot of the pane.
  footer: JSX.Element;
  conn: Connection;
  progress: ScanProgress | null;
  selected: KeyRef | null;
  onSelect: (key: KeyRef) => void;
  query: string;
  onQuery: (q: string) => void;
  version: number;
  onDeletePattern: (target: DeleteTarget) => void;
  onNewKey: () => void;
}

export interface FlatListProps {
  conn: Connection;
  progress: ScanProgress | null;
  selected: KeyRef | null;
  onSelect: (key: KeyRef) => void;
  version: number;
}

const refOf = (r: Row): KeyRef => ({ text: r.name, raw: r.raw });

export function createKeyListModel(props: KeyListProps) {
  const scanning = () => {
    const ph = props.progress?.phase;
    return ph === "scanning" || ph === "paused" || ph === "starting" || ph === "finishing" || (!props.progress && props.conn.scanning);
  };
  const isPlain = (q: string) => !/^\/.*\/$/.test(q) && !/[*?[]/.test(q.replace(/\\./g, ""));
  /// Without an index the pane is one list of SCAN pages (`ScanList`).
  const noIndex = () => props.conn.indexMode === "none";
  const searching = () => !noIndex() && props.query.trim() !== "";

  const onSearchKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") props.onQuery("");
    if (e.key === "Enter" && props.query.trim() && isPlain(props.query.trim())) props.onSelect(textKey(props.query.trim()));
  };

  return {
    scanning,
    searching,
    noIndex,
    hasIndex: () => !noIndex(),
    searchPlaceholder: () => (noIndex() ? "Pattern for SCAN MATCH" : "Search keys"),
    searchTitle: () =>
      noIndex()
        ? "Glob: user:42:*\nSubstring: session\nThe server walks every key for a pattern\nAn exact name and Enter opens that key"
        : "Glob: user:42:*\nSubstring: session\nRegex: /user:\\d+:cart/\nAn exact name and Enter opens that key",
    isTree: () => props.view === "tree",
    isList: () => props.view === "list",
    showTree: () => props.onView("tree"),
    showList: () => props.onView("list"),
    treeStyle: () => ({ display: !searching() && props.view === "tree" ? "flex" : "none" }),
    listVisible: () => !noIndex() && !searching() && props.view === "list",
    trimmedQuery: () => props.query.trim(),
    clearQuery: () => props.onQuery(""),
    onSearchKey,
  };
}

export function createFlatListModel(props: FlatListProps) {
  let scrollEl: HTMLDivElement | undefined;
  const [total, setTotal] = createSignal(props.conn.index.keys);
  const [exact, setExact] = createSignal(props.conn.index.status === "complete" && !props.conn.scanning);
  const [windows, setWindows] = createStore<Windows>({});
  const [meta, setMeta] = createStore<Record<string, KeyMeta | undefined>>({});
  const [settled, setSettled] = createSignal(true);
  const [requests, setRequests] = createSignal(0);
  const inFlight = new Set<number>();
  const order: number[] = [];
  let metaCount = 0;
  let lastScanRefresh = 0;

  const virtualizer = createVirtualizer({
    get count() {
      return total();
    },
    getScrollElement: () => scrollEl ?? null,
    estimateSize: () => ROW_HEIGHT,
    overscan: 16,
  });

  const rowAt = (i: number): Row | undefined => {
    const w = Math.floor(i / WINDOW);
    return windows[w]?.[i - w * WINDOW];
  };

  const fetchWindow = async (w: number) => {
    if (inFlight.has(w)) return;
    inFlight.add(w);
    setRequests((n) => n + 1);
    try {
      const page = await api.listRows(props.conn.id, w * WINDOW, WINDOW);
      batch(() => {
        setWindows(w, page.rows);
        setTotal(page.total);
        setExact(page.exact);
      });
      order.push(w);
      if (order.length > MAX_CACHED_WINDOWS) {
        const evict = order.shift()!;
        if (!visibleWindows().includes(evict)) setWindows(evict, undefined);
        else order.push(evict);
      }
    } catch (e) {
      console.error("list_rows", e);
    } finally {
      inFlight.delete(w);
    }
  };

  const visibleWindows = createMemo(() => {
    const items = virtualizer.getVirtualItems();
    if (items.length === 0) return [] as number[];
    const first = Math.floor(items[0].index / WINDOW);
    const last = Math.floor(items[items.length - 1].index / WINDOW);
    const ws: number[] = [];
    for (let w = first; w <= last; w++) ws.push(w);
    return ws;
  });

  // Request missing windows once scrolling has settled.
  createEffect(() => {
    if (!settled()) return;
    for (const w of visibleWindows()) {
      if (windows[w] === undefined) void fetchWindow(w);
    }
  });

  // Lazy metadata for the rows on screen, batched per scroll stop (and
  // again after a write, once the refetched windows are in place).
  createEffect(
    on([settled, visibleWindows, () => virtualizer.getVirtualItems().length, () => props.version], () => {
      if (!settled()) return;
      const t = setTimeout(async () => {
        const names: KeyRef[] = [];
        for (const item of virtualizer.getVirtualItems()) {
          const r = rowAt(item.index);
          if (r && meta[keyId(refOf(r))] === undefined) names.push(refOf(r));
        }
        if (names.length === 0) return;
        try {
          const metas = await api.keyMeta(props.conn.id, names);
          if (metaCount > 5000) {
            setMeta(produce((m) => { for (const k of Object.keys(m)) delete m[k]; }));
            metaCount = 0;
          }
          batch(() => names.forEach((n, i) => setMeta(keyId(n), metas[i])));
          metaCount += names.length;
        } catch (e) {
          console.error("key_meta", e);
        }
      }, META_DEBOUNCE_MS);
      onCleanup(() => clearTimeout(t));
    }),
  );

  // A write changed the index: refetch what is on screen, drop the rest.
  createEffect(
    on(
      () => props.version,
      (_v, prev) => {
        if (prev === undefined) return;
        const keep = visibleWindows();
        batch(() => {
          for (const k of Object.keys(windows)) if (!keep.includes(Number(k))) setWindows(Number(k), undefined);
          order.length = 0;
          setMeta(produce((m) => { for (const k of Object.keys(m)) delete m[k]; }));
          metaCount = 0;
        });
        for (const w of keep) void fetchWindow(w);
        if (keep.length === 0) void fetchWindow(0);
      },
    ),
  );

  // While a scan runs the rows shift; refresh what is visible at most every 2 s.
  createEffect(
    on(
      () => props.progress,
      (p) => {
        if (!p) return;
        const active = p.phase === "scanning" || p.phase === "finishing" || p.phase === "paused";
        const now = Date.now();
        // Nothing on screen yet: take every event so the first keys show as
        // soon as the first merge lands, not after the 2 s throttle.
        const empty = total() === 0 && p.indexKeys > 0;
        if (p.phase === "complete" || p.phase === "stopped" || (active && (empty || now - lastScanRefresh > SCAN_REFRESH_MS))) {
          lastScanRefresh = now;
          const keep = visibleWindows();
          batch(() => {
            for (const k of Object.keys(windows)) if (!keep.includes(Number(k))) setWindows(Number(k), undefined);
            order.length = 0;
            setTotal(Math.max(total(), p.indexKeys));
            setExact(p.phase === "complete");
          });
          for (const w of keep) void fetchWindow(w);
        }
      },
    ),
  );

  onMount(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const onScroll = () => {
      setSettled(false);
      clearTimeout(timer);
      timer = setTimeout(() => setSettled(true), SCROLL_SETTLE_MS);
    };
    scrollEl!.addEventListener("scroll", onScroll, { passive: true });
    onCleanup(() => {
      scrollEl?.removeEventListener("scroll", onScroll);
      clearTimeout(timer);
    });
  });

  const metaAt = (i: number) => {
    const r = rowAt(i);
    return r ? meta[keyId(refOf(r))] : undefined;
  };

  return {
    setScrollEl: (el: HTMLDivElement) => (scrollEl = el),
    items: () => virtualizer.getVirtualItems(),
    innerStyle: () => ({ height: `${virtualizer.getTotalSize()}px` }),
    rowStyle: (start: number) => ({ transform: `translateY(${start}px)` }),
    isPlaceholder: (i: number) => !rowAt(i),
    isSelected: (i: number) => !!rowAt(i) && sameKey(refOf(rowAt(i)!), props.selected),
    select: (i: number) => rowAt(i) && props.onSelect(refOf(rowAt(i)!)),
    isBinary: (i: number) => !!rowAt(i)?.raw,
    rowTitle: (i: number) => rowAt(i)?.name,
    rowName: (i: number) => rowAt(i)?.name ?? "…",
    rowType: (i: number) => (metaAt(i)?.exists === false ? "gone" : (metaAt(i)?.keyType ?? "")),
    rowTtl: (i: number) => fmtTtl(metaAt(i)?.ttl ?? null),
    rowMem: (i: number) => fmtBytes(metaAt(i)?.mem ?? null),
    cacheTitle: () => `${fmtCount(Object.keys(windows).length)} windows cached, ${requests()} window requests`,
    approxMark: () => (exact() ? "" : "≥ "),
    totalText: () => fmtCount(total()),
  };
}
