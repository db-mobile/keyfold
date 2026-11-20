// State and actions of the key list of a connection without an index
// (CONCEPT 7, "no index"). The markup is in `ScanList.tsx`. Keys arrive in
// pages of plain SCAN, in the server's order, and only when asked for: the
// first page on opening, the next ones through the "Load more" row. There
// is no total and no sorting; the search box becomes the MATCH pattern of
// the walk. The list holds at most 10,000 rows; beyond that it continues
// with an empty list, so the webview never holds the keyspace.

import { createVirtualizer } from "@tanstack/solid-virtual";
import { batch, createEffect, createSignal, on, onCleanup } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { api, fmtBytes, fmtCount, fmtTtl, type Connection, type KeyMeta, type KeyRef, type KeysPage } from "./api";
import { isBinary, keyId, sameKey } from "./keyRef";
import { LIST_ROW } from "./ui/density";

const ROW_HEIGHT = LIST_ROW;
const DEBOUNCE_MS = 300;
const META_DEBOUNCE_MS = 150;
/// Rows the pane holds at most.
const MAX_ROWS = 10_000;

export interface ScanListProps {
  conn: Connection;
  /// The search box: a glob or a substring for SCAN MATCH, or nothing.
  query: string;
  selected: KeyRef | null;
  onSelect: (key: KeyRef) => void;
}

export function createScanListModel(props: ScanListProps) {
  let scrollEl: HTMLDivElement | undefined;
  const [rows, setRows] = createSignal<KeyRef[]>([]);
  const [page, setPage] = createSignal<KeysPage | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  /// Rows dropped by "continue", so the caption still counts them.
  const [dropped, setDropped] = createSignal(0);
  const [meta, setMeta] = createStore<Record<string, KeyMeta | undefined>>({});
  const [settled, setSettled] = createSignal(true);
  let generation = 0;

  const virtualizer = createVirtualizer({
    get count() {
      // the last row is the action row: load more, continue, or the end
      return rows().length + 1;
    },
    getScrollElement: () => scrollEl ?? null,
    estimateSize: () => ROW_HEIGHT,
    overscan: 16,
  });

  const load = async (restart: boolean) => {
    if (loading() && !restart) return;
    const gen = restart ? ++generation : generation;
    setLoading(true);
    if (restart) {
      batch(() => {
        setRows([]);
        setPage(null);
        setDropped(0);
        setMeta(produce((m) => { for (const k of Object.keys(m)) delete m[k]; }));
      });
    }
    try {
      const p = await api.keysPage(props.conn.id, props.query || null, restart);
      if (gen !== generation) return;
      batch(() => {
        setPage(p);
        setRows([...rows(), ...p.keys]);
        setError(null);
      });
    } catch (e) {
      if (gen === generation) setError(String(e));
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  // The pattern changed (or the pane opened): a new walk, after a pause.
  createEffect(
    on(
      () => props.query,
      () => {
        const t = setTimeout(() => void load(true), props.query ? DEBOUNCE_MS : 0);
        onCleanup(() => clearTimeout(t));
      },
    ),
  );

  // Lazy metadata for the rows on screen.
  createEffect(
    on([settled, rows, () => virtualizer.getVirtualItems().length], () => {
      if (!settled()) return;
      const t = setTimeout(async () => {
        const names: KeyRef[] = [];
        for (const item of virtualizer.getVirtualItems()) {
          const r = rows()[item.index];
          if (r && meta[keyId(r)] === undefined) names.push(r);
        }
        if (names.length === 0) return;
        try {
          const metas = await api.keyMeta(props.conn.id, names);
          batch(() => names.forEach((n, i) => setMeta(keyId(n), metas[i])));
        } catch (e) {
          console.error("key_meta", e);
        }
      }, META_DEBOUNCE_MS);
      onCleanup(() => clearTimeout(t));
    }),
  );

  createEffect(() => {
    const el = scrollEl;
    if (!el) return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const onScroll = () => {
      setSettled(false);
      clearTimeout(timer);
      timer = setTimeout(() => setSettled(true), 80);
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    onCleanup(() => {
      el.removeEventListener("scroll", onScroll);
      clearTimeout(timer);
    });
  });

  const rowAt = (i: number): KeyRef | undefined => rows()[i];
  const metaAt = (i: number) => (rowAt(i) ? meta[keyId(rowAt(i)!)] : undefined);
  const done = () => page()?.done ?? false;
  const full = () => rows().length >= MAX_ROWS;
  const filtered = () => props.query !== "";

  /// The walk goes on with an empty list: what was shown is let go.
  const proceed = () => {
    batch(() => {
      setDropped(dropped() + rows().length);
      setRows([]);
      setMeta(produce((m) => { for (const k of Object.keys(m)) delete m[k]; }));
    });
    void load(false);
  };

  /// How much of the keyspace the server has walked, when DBSIZE is known.
  const walked = () => {
    const p = page();
    const size = props.conn.dbsize;
    if (!p || !size) return "";
    if (p.done) return ", end of the keyspace";
    return `, about ${Math.min(99, (p.examined / size) * 100).toFixed(p.examined / size < 0.1 ? 1 : 0)} % walked`;
  };

  return {
    setScrollEl: (el: HTMLDivElement) => (scrollEl = el),
    items: () => virtualizer.getVirtualItems(),
    innerStyle: () => ({ height: `${virtualizer.getTotalSize()}px` }),
    rowStyle: (start: number) => ({ transform: `translateY(${start}px)` }),
    isAction: (i: number) => i === rows().length,
    isSelected: (i: number) => sameKey(rowAt(i), props.selected),
    select: (i: number) => rowAt(i) && props.onSelect(rowAt(i)!),
    isBinary: (i: number) => isBinary(rowAt(i)),
    rowTitle: (i: number) => rowAt(i)?.text,
    rowName: (i: number) => rowAt(i)?.text ?? "",
    rowType: (i: number) => (metaAt(i)?.exists === false ? "gone" : (metaAt(i)?.keyType ?? "")),
    rowTtl: (i: number) => fmtTtl(metaAt(i)?.ttl ?? null),
    rowMem: (i: number) => fmtBytes(metaAt(i)?.mem ?? null),

    // the action row
    loading,
    canLoadMore: () => !loading() && !done() && !full() && !error(),
    loadMore: () => void load(false),
    loadMoreLabel: () => {
      const p = page();
      // a pattern can leave a page short or empty with more to walk
      if (p && filtered() && p.keys.length === 0) return "Nothing in that stretch. Load more";
      return "Load more";
    },
    canProceed: () => !loading() && !done() && full(),
    proceed,
    proceedLabel: () => `Continue (clears these ${fmtCount(MAX_ROWS)} rows)`,
    atEnd: () => !loading() && done(),
    endLabel: () => (rows().length + dropped() === 0 ? (filtered() ? "No key matches" : "The database is empty") : "End of the keyspace"),
    canRetry: () => !loading() && !!error(),
    retry: () => void load(page() === null),

    error,
    restart: () => void load(true),
    caption: () => `${fmtCount(rows().length + dropped())} keys loaded${walked()}`,
    captionTitle: () => {
      const p = page();
      if (!p) return "";
      const node = p.nodes > 1 && p.node ? `, node ${Math.min(p.nodeAt + 1, p.nodes)} of ${p.nodes} (${p.node})` : "";
      return `SCAN${p.mode ? ` MATCH (${p.mode})` : ""}, in the server's order${node}`;
    },
    note: () => (filtered() ? "Matches, as the server walks its keys" : "Keys in the server's order"),
    noteTitle: () =>
      filtered()
        ? "The server walks every key for a pattern; only the matches come back, a stretch at a time."
        : "This connection keeps no index: no tree, no counts, no search over all keys. Choose another index in the scan options below.",
  };
}
