// State and actions of the search results (CONCEPT 5.3). The markup is in
// `SearchResults.tsx`. Matches arrive in windows of 200 keyed on the last
// row (keyset paging); scrolling to the end loads the next window, up to a
// cap, so the webview never holds the whole match set. A newer query
// cancels the search, count or verification still running for the old one.

import { createVirtualizer } from "@tanstack/solid-virtual";
import { batch, createEffect, createSignal, on, onCleanup } from "solid-js";
import { createStore, produce } from "solid-js/store";
import {
  api,
  fmtBytes,
  fmtCount,
  fmtTtl,
  type Connection,
  type DeleteTarget,
  type FtsProgress,
  type KeyMeta,
  type KeyRef,
  type SearchPage,
  type SearchRow,
  type VerifyReport,
} from "./api";
import { keyId, sameKey } from "./keyRef";
import { LIST_ROW } from "./ui/density";

const ROW_HEIGHT = LIST_ROW;
const DEBOUNCE_MS = 150;
/// Matches the pane holds at most; beyond this the user refines the query.
const MAX_ROWS = 10_000;
/// A substring search slower than this earns the trigram offer (CONCEPT 5.3).
const SLOW_MS = 500;

export interface SearchResultsProps {
  conn: Connection;
  query: string;
  scanning: boolean;
  selected: KeyRef | null;
  onSelect: (key: KeyRef) => void;
  /// Bumped when the index changed behind the pane's back (a write).
  version: number;
  onDeletePattern: (target: DeleteTarget) => void;
}

const refOf = (r: SearchRow): KeyRef => ({ text: r.name, raw: r.raw });

export function createSearchResultsModel(props: SearchResultsProps) {
  let scrollEl: HTMLDivElement | undefined;
  const [rows, setRows] = createSignal<SearchRow[]>([]);
  const [page, setPage] = createSignal<SearchPage | null>(null);
  const [count, setCount] = createSignal<number | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(false);
  const [verify, setVerify] = createSignal<VerifyReport | null>(null);
  const [verifying, setVerifying] = createSignal(false);
  const [fts, setFts] = createSignal<FtsProgress | null>(null);
  const [meta, setMeta] = createStore<Record<string, KeyMeta | undefined>>({});
  const [settled, setSettled] = createSignal(true);
  let generation = 0;

  const virtualizer = createVirtualizer({
    get count() {
      const n = rows().length;
      const p = page();
      return p && !p.done && !p.cancelled && n < MAX_ROWS ? n + 1 : n;
    },
    getScrollElement: () => scrollEl ?? null,
    estimateSize: () => ROW_HEIGHT,
    overscan: 16,
  });

  const run = async (first: boolean) => {
    const q = props.query;
    const gen = generation;
    const last = first ? null : rows()[rows().length - 1];
    setLoading(true);
    try {
      const p = await api.search(props.conn.id, q, last ? last.name : null, last ? last.raw : null, first);
      if (gen !== generation) return;
      batch(() => {
        setPage(p);
        setRows(first ? p.rows : [...rows(), ...p.rows]);
        setError(null);
      });
      if (first && !p.cancelled) {
        if (p.done) setCount(p.rows.length);
        else {
          const n = await api.searchCount(props.conn.id, q).catch(() => null);
          if (gen === generation) setCount(n);
        }
      }
    } catch (e) {
      if (gen === generation) setError(String(e));
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  // A new query: cancel the old one, start over after a short pause.
  createEffect(
    on(
      () => props.query,
      (q) => {
        generation++;
        void api.searchCancel(props.conn.id);
        batch(() => {
          setRows([]);
          setPage(null);
          setCount(null);
          setVerify(null);
          setError(null);
        });
        if (!q) return;
        const t = setTimeout(() => void run(true), DEBOUNCE_MS);
        onCleanup(() => clearTimeout(t));
      },
    ),
  );

  // The index changed behind our back (a write): the same query, again.
  createEffect(
    on(
      () => props.version,
      (_v, prev) => {
        if (prev === undefined || !props.query) return;
        generation++;
        setMeta(produce((m) => { for (const k of Object.keys(m)) delete m[k]; }));
        void run(true);
      },
    ),
  );

  // The placeholder row scrolled into view loads the next window.
  createEffect(() => {
    if (!settled() || loading()) return;
    const n = rows().length;
    for (const item of virtualizer.getVirtualItems()) {
      if (item.index === n && page() && !page()!.done && n < MAX_ROWS) {
        void run(false);
        break;
      }
    }
  });

  // Lazy metadata for the rows on screen.
  createEffect(
    on([settled, rows, () => virtualizer.getVirtualItems().length], () => {
      if (!settled()) return;
      const t = setTimeout(async () => {
        const names: KeyRef[] = [];
        for (const item of virtualizer.getVirtualItems()) {
          const r = rows()[item.index];
          if (r && meta[keyId(refOf(r))] === undefined) names.push(refOf(r));
        }
        if (names.length === 0) return;
        try {
          const metas = await api.keyMeta(props.conn.id, names);
          batch(() => names.forEach((n, i) => setMeta(keyId(n), metas[i])));
        } catch (e) {
          console.error("key_meta", e);
        }
      }, DEBOUNCE_MS);
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

  createEffect(() => {
    let unlisten: (() => void) | undefined;
    void api.onFtsProgress((p) => {
      if (p.session !== props.conn.id) return;
      setFts(p);
      api.log("info", `fts ${p.phase} ${p.done}/${p.total}${p.error ? ` ${p.error}` : ""}`);
      if (p.phase === "ready" || p.phase === "dropped") {
        // the same query now takes the other path
        generation++;
        void run(true);
      }
    }).then((u) => (unlisten = u));
    onCleanup(() => unlisten?.());
  });

  const runVerify = async () => {
    const gen = generation;
    setVerifying(true);
    try {
      const r = await api.searchVerify(props.conn.id, props.query);
      if (gen !== generation) return;
      setVerify(r);
      api.log("info", `verify ${JSON.stringify(r)}`);
      if (r.added > 0 || r.removed > 0) {
        generation++;
        void run(true);
      }
    } catch (e) {
      if (gen === generation) setError(String(e));
    } finally {
      if (gen === generation) setVerifying(false);
    }
  };

  const ftsBusy = () => fts()?.phase === "building" || fts()?.phase === "dropping";
  const summary = () => {
    const p = page();
    if (!p) return loading() ? "Searching…" : "";
    const n = count();
    const total = n === null ? `${p.done ? "" : "≥ "}${fmtCount(rows().length)}` : fmtCount(n);
    const parts = [`${total} ${n === 1 ? "match" : "matches"}`, `${p.tookMs.toFixed(0)} ms`];
    if (p.viaFts) parts.push("via trigram index");
    if (p.cancelled) parts.push("cancelled");
    if (rows().length >= MAX_ROWS && !p.done) parts.push(`showing the first ${fmtCount(MAX_ROWS)}, refine the query`);
    return parts.join(", ");
  };

  const modeClass = () => {
    const m = page()?.mode;
    return m === "glob" ? "success" : m === "regex" ? "warning" : m === "substring" ? "accent" : "";
  };

  /// The parts of the note under the actions once a verification returned.
  const verifyNote = () => {
    const v = verify();
    if (!v) return null;
    return {
      label: v.complete ? "Verified" : "Partially verified",
      matched: fmtCount(v.matched),
      calls: v.calls,
      added: fmtCount(v.added),
      removed: fmtCount(v.removed),
      truncated: v.truncated ? ", stopped at the limit" : "",
      took: v.tookMs.toFixed(0),
    };
  };

  const rowAt = (i: number): SearchRow | undefined => rows()[i];
  const metaAt = (i: number) => (rowAt(i) ? meta[keyId(refOf(rowAt(i)!))] : undefined);

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
    rowName: (i: number) => rowAt(i)?.name ?? (loading() ? "…" : ""),
    rowType: (i: number) => (metaAt(i)?.exists === false ? "gone" : (metaAt(i)?.keyType ?? "")),
    rowTtl: (i: number) => fmtTtl(metaAt(i)?.ttl ?? rowAt(i)?.ttl ?? null),
    rowMem: (i: number) => fmtBytes(metaAt(i)?.mem ?? rowAt(i)?.mem ?? null),
    badgeClass: () => `badge ${modeClass()}`,
    modeText: () => page()?.mode ?? "…",
    summary,
    ftsBusy,
    ftsPhase: () => fts()!.phase,
    ftsPercent: () => (fts()!.total ? `${((fts()!.done / fts()!.total) * 100).toFixed(0)} %` : ""),
    canVerify: () => page()?.verifiable && !props.scanning,
    verifying,
    verifyLabel: () => (verifying() ? "Verifying…" : "Verify against server"),
    runVerify,
    canEnableFts: () => page() && !page()!.fts && page()!.ftsApplicable && page()!.tookMs > SLOW_MS && !props.scanning && !ftsBusy(),
    enableFts: () => api.searchFts(props.conn.id, true).catch((e) => setError(String(e))),
    canDisableFts: () => page()?.fts && !props.scanning && !ftsBusy(),
    disableFts: () => api.searchFts(props.conn.id, false).catch((e) => setError(String(e))),
    canDeleteAll: () => count() !== null && count()! > 0 && !props.scanning,
    countText: () => fmtCount(count()!),
    deletePattern: () => props.onDeletePattern({ query: props.query, prefix: null }),
    verifyNote,
    error,
    noMatches: () => page()?.done && rows().length === 0 && !loading(),
    noMatchesHint: () =>
      props.conn.indexMode === "onDemand" && props.conn.index.fullAt === null
        ? "Only the sampled and scanned part of the keyspace is in the index. Verify against the server to look in the rest."
        : "Keys are matched against the index. Verify against the server if the index may be stale.",
  };
}
