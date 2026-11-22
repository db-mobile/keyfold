// State and actions of the namespace tree: a virtualised, flattened view
// over the `nodes` table. The markup is in `Tree.tsx`. Every expanded node
// holds the windows of children loaded so far (200 per request); scrolling
// past the last loaded child loads the next window. Levels with at least
// 100 children are folded (CONCEPT 5.6): the backend decides from the shape
// counters and the tree shows one row per fold with the structure beneath,
// buckets, a plain list and a filter box, next to the literal siblings.
// Counts are refreshed for the branches on screen while a scan runs and
// written into the store field by field.

import { createVirtualizer, type VirtualItem } from "@tanstack/solid-virtual";
import { batch, createEffect, createMemo, createSignal, on, onCleanup, onMount } from "solid-js";
import { createStore } from "solid-js/store";
import {
  api,
  fmtCount,
  WINDOW,
  type ChildFilter,
  type Connection,
  type DeleteTarget,
  type FoldInfo,
  type KeyRef,
  type ScanProgress,
  type TreeAfter,
  type TreeNode,
  type TreeRoot,
  type TreeSort,
} from "./api";
import { ask } from "./dialogModel";
import { globEscape, keyId, sameKey } from "./keyRef";
import { ago } from "./scanModel";
import { toast } from "./ui/toastModel";
import { TREE_ROW } from "./ui/density";

const ROW_HEIGHT = TREE_ROW;
const MAX_LOADED_PER_NODE = 10_000;
const SCROLL_SETTLE_MS = 80;
const REFRESH_MS = 1000;
/// A level folds at this many children; it unfolds again only below 50
/// (the decision is held until the node is collapsed).
const FOLD_AT = 100;
const SUMMARY_ROWS = 12;

export interface TreeProps {
  conn: Connection;
  progress: ScanProgress | null;
  selected: KeyRef | null;
  onSelect: (key: KeyRef) => void;
  /// Development aid: expand this prefix (and its first fold and bucket) once the root is loaded.
  autoexpand?: string | null;
  /// Development aid: change the sort to this once the automatic expand is through.
  autosort?: string | null;
  /// Bumped when the index changed behind the tree's back (a write).
  version: number;
  onSearch: (query: string) => void;
  onDeletePattern: (target: DeleteTarget) => void;
  /// "Scan everything", the scan-root action of an on-demand index.
  onScanAll: () => void;
  /// The connection's numbers changed (a scan was queued).
  onRefresh: () => void;
}

export interface TwistyProps {
  show: boolean;
  open: boolean;
}

interface Branch {
  rows: TreeNode[];
  expanded: boolean;
  /// All children loaded (the last window came back short).
  complete: boolean;
  loading: boolean;
  /// Where the rows come from: the node's prefix plus an optional filter.
  prefix: string;
  raw: string | null;
  filter: ChildFilter | null;
  /// Numeric sort for "show as list" of numeric ids, else the global sort.
  sort: TreeSort | null;
  /// The node branch a bucket, list or filter branch belongs to; null for a node's own branch.
  parent: string | null;
}

export type FlatRow =
  | { kind: "node"; depth: number; branch: string; index: number }
  | { kind: "more"; depth: number; branch: string }
  | { kind: "folding"; depth: number; branch: string }
  | { kind: "fold"; depth: number; branch: string; fold: number }
  | { kind: "summary"; depth: number; branch: string; fold: number; index: number }
  | { kind: "bucket"; depth: number; branch: string; fold: number; index: number }
  | { kind: "list"; depth: number; branch: string; fold: number }
  | { kind: "filter"; depth: number; branch: string; fold: number };

export interface TreeMenu {
  x: number;
  y: number;
  node: TreeNode;
}

const branchKey = (prefix: string, raw: string | null) => (raw ? `raw:${raw}` : prefix);

export function createTreeModel(props: TreeProps) {
  const [menu, setMenu] = createSignal<TreeMenu | null>(null);
  let scrollEl: HTMLDivElement | undefined;
  const [branches, setBranches] = createStore<Record<string, Branch | undefined>>({});
  const [folds, setFolds] = createStore<Record<string, FoldInfo | "loading" | undefined>>({});
  const [open, setOpen] = createStore<Record<string, boolean | undefined>>({});
  const [queries, setQueries] = createStore<Record<string, { text: string; count: number | null } | undefined>>({});
  const [root, setRoot] = createSignal<TreeRoot | null>(null);
  const [sort, setSort] = createSignal<TreeSort>("count");
  const [settled, setSettled] = createSignal(true);
  const [requests, setRequests] = createSignal(0);
  let lastRefresh = 0;
  /// Prefixes whose scan the user asked for or turned down in this session.
  const [asked, setAsked] = createStore<Record<string, "wanted" | "declined" | undefined>>({});
  const onDemand = () => props.conn.indexMode === "onDemand";

  const emptyBranch = (prefix: string, raw: string | null, filter: ChildFilter | null = null, sortOverride: TreeSort | null = null, parent: string | null = null): Branch => ({
    rows: [],
    expanded: false,
    complete: false,
    loading: false,
    prefix,
    raw,
    filter,
    sort: sortOverride,
    parent,
  });

  const loadWindow = async (key: string, after: TreeAfter | null) => {
    const b = branches[key];
    if (!b || b.loading) return;
    setBranches(key, "loading", true);
    setRequests((n) => n + 1);
    try {
      const w = await api.treeChildren(props.conn.id, b.prefix, b.raw, after, b.sort ?? sort(), b.filter);
      const cur = branches[key]!;
      const rows = after ? [...cur.rows, ...w.nodes] : w.nodes;
      batch(() => {
        setBranches(key, "rows", rows);
        setBranches(key, "complete", w.nodes.length < WINDOW || rows.length >= MAX_LOADED_PER_NODE);
        setBranches(key, "loading", false);
      });
    } catch (e) {
      console.error("tree_children", e);
      setBranches(key, "loading", false);
    }
  };

  /// Refetch the windows a branch has already loaded and apply count changes in place.
  const refreshBranch = async (key: string) => {
    const cur = branches[key];
    if (!cur) return;
    if (cur.rows.length === 0 && key !== "") return;
    let after: TreeAfter | null = null;
    let fresh: TreeNode[] = [];
    const windows = Math.max(1, Math.ceil(cur.rows.length / WINDOW));
    for (let i = 0; i < windows; i++) {
      const w = await api.treeChildren(props.conn.id, cur.prefix, cur.raw, after, cur.sort ?? sort(), cur.filter);
      fresh = fresh.concat(w.nodes);
      if (w.nodes.length < WINDOW) break;
      const last = w.nodes[w.nodes.length - 1];
      after = { segment: last.segment, raw: last.raw, count: last.count, segNum: last.segNum };
    }
    const now = branches[key];
    if (!now) return;
    const sameShape = fresh.length === now.rows.length && fresh.every((n, i) => n.segment === now.rows[i].segment && n.raw === now.rows[i].raw);
    batch(() => {
      if (sameShape) {
        fresh.forEach((n, i) => {
          const o = now.rows[i];
          if (o.count !== n.count) setBranches(key, "rows", i, "count", n.count);
          if (o.scannedAt !== n.scannedAt) setBranches(key, "rows", i, "scannedAt", n.scannedAt);
          if (o.scan !== n.scan) setBranches(key, "rows", i, "scan", n.scan);
          if (o.children !== n.children) setBranches(key, "rows", i, "children", n.children);
          if (o.leaf !== n.leaf) setBranches(key, "rows", i, "leaf", n.leaf);
        });
      } else {
        setBranches(key, "rows", fresh);
        setBranches(key, "complete", fresh.length < windows * WINDOW);
      }
    });
  };

  /// Ask the backend whether and how a level folds; literal siblings become
  /// the branch's rows, the folds render above them.
  const decideFold = async (key: string, prefix: string, raw: string | null) => {
    setFolds(key, "loading");
    setRequests((n) => n + 1);
    try {
      const info = await api.treeFold(props.conn.id, prefix, raw);
      batch(() => {
        setFolds(key, info);
        if (info.folded) {
          setBranches(key, { ...branches[key]!, rows: info.literals, complete: true, loading: false });
        }
      });
      if (!info.folded) void loadWindow(key, null);
    } catch (e) {
      console.error("tree_fold", e);
      setFolds(key, undefined);
      void loadWindow(key, null);
    }
  };

  /// Scan the subtree under a node (CONCEPT 4 "subtree rescan", CONCEPT 7
  /// "on demand"). The server walks its whole key table for it, so a
  /// prod-tagged connection asks first; `explicit` is the menu action,
  /// which asks again after a "no".
  const scanSubtree = async (n: TreeNode, explicit: boolean) => {
    const key = branchKey(n.prefix, n.prefixRaw);
    if (!explicit && asked[key]) return;
    if (props.conn.tag === "prod") {
      const ok = await ask({
        title: `Scan ${n.prefix}*?`,
        body: `The server walks all ${props.conn.dbsize === null ? "" : `${fmtCount(props.conn.dbsize)} `}keys to find the ones under this prefix; only those cross the network. It runs with the ${props.conn.scanProfile} profile and can be paused.`,
        confirm: "Scan",
      });
      if (!ok) {
        setAsked(key, "declined");
        return;
      }
    }
    setAsked(key, "wanted");
    try {
      const outcome = await api.scanSubtree(props.conn.id, { text: n.prefix, raw: n.prefixRaw }, true);
      void api.log("info", `scan_subtree ${n.prefix} ${outcome}`);
      if (explicit && outcome !== "started") toast(outcome === "queued" ? `Scan of ${n.prefix}* queued` : `A running scan already covers ${n.prefix}*`);
      props.onRefresh();
      refreshVisible();
    } catch (e) {
      setAsked(key, undefined);
      toast(`Scan of ${n.prefix}*: ${e}`, "error");
    }
  };

  const toggle = (n: TreeNode) => {
    if (n.key) props.onSelect(n.key);
    if (n.children === 0) return;
    const key = branchKey(n.prefix, n.prefixRaw);
    const b = branches[key];
    // on demand, opening a subtree the index knows only in part scans it
    if (!b?.expanded && onDemand() && n.scannedAt === null && n.scan === "idle") void scanSubtree(n, false);
    if (b?.expanded) {
      setBranches(key, "expanded", false);
      // collapsing releases the fold decision (hysteresis ends here)
      if (n.children < FOLD_AT) setFolds(key, undefined);
      return;
    }
    if (!b) setBranches(key, { ...emptyBranch(n.prefix, n.prefixRaw), expanded: true });
    else setBranches(key, "expanded", true);
    if (b && (b.rows.length > 0 || folds[key])) return;
    if (n.children >= FOLD_AT) void decideFold(key, n.prefix, n.prefixRaw);
    else void loadWindow(key, null);
  };

  /// A sub-branch id. NUL separates it from the node's id: `|` is one of the delimiters the backend
  /// detects, so a node prefix such as `a|f0` is ordinary, while one holding NUL is not.
  const subKey = (key: string, suffix: string) => `${key}\u0000${suffix}`;

  const toggleSub = (key: string, suffix: string, filter: ChildFilter | null, sortOverride: TreeSort | null) => {
    const k = subKey(key, suffix);
    const parent = branches[key]!;
    const b = branches[k];
    if (b?.expanded) {
      setBranches(k, "expanded", false);
      return;
    }
    if (!b) {
      setBranches(k, { ...emptyBranch(parent.prefix, parent.raw, filter, sortOverride, key), expanded: true });
      void loadWindow(k, null);
    } else {
      setBranches(k, "expanded", true);
    }
  };

  const foldOpen = (key: string, i: number) => !!open[subKey(key, `f${i}`)];

  const setQuery = (key: string, i: number, text: string) => {
    const qk = subKey(key, `f${i}|q`);
    setQueries(qk, { text, count: null });
    const parent = branches[key]!;
    const filter: ChildFilter | null = text ? { segMin: text, segMax: text + "￿" } : null;
    if (!text) {
      setBranches(qk, undefined);
      return;
    }
    setBranches(qk, { ...emptyBranch(parent.prefix, parent.raw, filter, null, key), expanded: true });
    void loadWindow(qk, null);
    void api
      .treeCount(props.conn.id, parent.prefix, parent.raw, filter!)
      .then((c) => queries[qk]?.text === text && setQueries(qk, "count", c))
      .catch(() => {});
  };

  const flat = createMemo<FlatRow[]>(() => {
    const out: FlatRow[] = [];
    const walkBranch = (key: string, depth: number) => {
      const b = branches[key];
      if (!b) return;
      for (let i = 0; i < b.rows.length; i++) {
        out.push({ kind: "node", depth, branch: key, index: i });
        const n = b.rows[i];
        const childKey = branchKey(n.prefix, n.prefixRaw);
        if (n.children > 0 && branches[childKey]?.expanded) walkLevel(childKey, depth + 1);
      }
      if (!b.complete) out.push({ kind: "more", depth, branch: key });
    };
    const walkLevel = (key: string, depth: number) => {
      const f = folds[key];
      if (f === "loading") {
        out.push({ kind: "folding", depth, branch: key });
        return;
      }
      if (f && f.folded) {
        f.folds.forEach((fold, i) => {
          out.push({ kind: "fold", depth, branch: key, fold: i });
          if (!foldOpen(key, i)) return;
          fold.summary.slice(0, SUMMARY_ROWS).forEach((_e, j) => out.push({ kind: "summary", depth: depth + 1, branch: key, fold: i, index: j }));
          fold.buckets.forEach((_b, j) => {
            out.push({ kind: "bucket", depth: depth + 1, branch: key, fold: i, index: j });
            const bk = subKey(key, `f${i}|b${j}`);
            if (branches[bk]?.expanded) walkBranch(bk, depth + 2);
          });
          out.push({ kind: "list", depth: depth + 1, branch: key, fold: i });
          const lk = subKey(key, `f${i}|list`);
          if (branches[lk]?.expanded) walkBranch(lk, depth + 2);
          out.push({ kind: "filter", depth: depth + 1, branch: key, fold: i });
          const qk = subKey(key, `f${i}|q`);
          if (branches[qk]?.expanded) walkBranch(qk, depth + 2);
        });
      }
      walkBranch(key, depth);
    };
    walkLevel("", 0);
    return out;
  });

  const virtualizer = createVirtualizer({
    get count() {
      return flat().length;
    },
    getScrollElement: () => scrollEl ?? null,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  });

  const loadRoot = () =>
    api
      .treeRoot(props.conn.id)
      .then((r) => {
        setRoot(r);
        if (!branches[""]) setBranches("", { ...emptyBranch("", null), expanded: true });
        return loadWindow("", null);
      })
      .then(() => {
        if (props.autoexpand) return autoExpand(props.autoexpand).then(() => (props.autosort ? autoSort(props.autosort) : undefined));
      })
      .catch((e) => console.error("tree_root", e));

  const autoExpand = async (prefix: string) => {
    let n = branches[""]?.rows.find((x) => x.prefix === prefix);
    // an index that is only now being filled (a sample, a first scan) shows the prefix a moment later
    for (let i = 0; i < 20 && !n; i++) {
      await new Promise((r) => setTimeout(r, 500));
      await loadWindow("", null);
      n = branches[""]?.rows.find((x) => x.prefix === prefix);
    }
    if (!n) {
      void api.log("warn", `autoexpand: ${prefix} not among the root rows yet`);
      return;
    }
    toggle(n);
    const key = branchKey(n.prefix, n.prefixRaw);
    for (let i = 0; i < 600 && folds[key] === "loading"; i++) await new Promise((r) => setTimeout(r, 100));
    const info = folds[key];
    if (!info || info === "loading" || !info.folded) {
      void api.log("info", `autoexpand: ${prefix} not folded`);
      return;
    }
    setOpen(subKey(key, "f0"), true);
    const b = info.folds[0].buckets[0];
    if (b) {
      const filter: ChildFilter =
        b.segNumMin !== null ? { segNumMin: b.segNumMin!, segNumMax: b.segNumMax! } : { segMin: b.segMin!, segMinRaw: b.segMinRaw, segMax: b.segMax!, segMaxRaw: b.segMaxRaw };
      toggleSub(key, "f0|b0", filter, b.segNumMin !== null ? "numeric" : "name");
    }
    setQuery(key, 0, info.folds[0].samples[0]?.slice(0, 2) ?? "1");
    void api.log("info", `autoexpand: ${prefix} folded as ${info.folds.map((f) => `${f.label} (${f.members})`).join(", ")}; ${info.evidence}; literals ${info.literals.length}; computed ${info.computedMs.toFixed(0)} ms; summary ${info.folds[0].summary.slice(0, 4).map((e) => `${e.segment} ${(e.coverage * 100).toFixed(1)}%`).join(", ")}; buckets ${info.folds[0].buckets.length}`);
  };

  /// Development aid: change the sort once and log the rows before and after. A folded level must keep its row count.
  const autoSort = async (value: string) => {
    const counts = () => `${flat().length} rows, ${Object.values(branches).filter((b) => b?.loading).length} loading`;
    await new Promise((r) => setTimeout(r, 1500));
    void api.log("info", `autosort: before ${sort()} -> ${value}: ${counts()}`);
    setSort(value as TreeSort);
    await new Promise((r) => setTimeout(r, 1500));
    void api.log("info", `autosort: after ${value}: ${counts()}`);
  };

  onMount(() => {
    void loadRoot();
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

  // Sort change: reload every loaded, unfolded branch from the start.
  createEffect(
    on(sort, (_s, prev) => {
      if (prev === undefined) return;
      for (const key of Object.keys(branches)) {
        const b = branches[key];
        const fold = b?.parent === null ? folds[key] : undefined;
        // a folded node branch holds the fold's literals, not a window: nothing to reload
        if (!b || b.sort || (fold && fold !== "loading" && fold.folded)) continue;
        setBranches(key, { ...b, rows: [], complete: false });
        if (b.expanded) void loadWindow(key, null);
      }
    }),
  );

  // "more" rows that come into view load the next window of their branch.
  createEffect(() => {
    if (!settled()) return;
    const rows = flat();
    for (const item of virtualizer.getVirtualItems()) {
      const r = rows[item.index];
      if (!r || r.kind !== "more") continue;
      const b = branches[r.branch];
      if (!b || b.loading || b.complete) continue;
      const last = b.rows[b.rows.length - 1];
      if (!last) continue;
      void loadWindow(r.branch, { segment: last.segment, raw: last.raw, count: last.count, segNum: last.segNum });
    }
  });

  /// Refresh the root and the branches that have rows on screen.
  const refreshVisible = () => {
    const rows = flat();
    const visible = new Set<string>([""]);
    for (const item of virtualizer.getVirtualItems()) {
      const r = rows[item.index];
      if (r && r.kind === "node") visible.add(r.branch);
    }
    void api.treeRoot(props.conn.id).then(setRoot).catch(() => {});
    for (const key of visible) void refreshBranch(key);
  };

  // Live counts: once a second while a scan runs, and once when it ends,
  // refresh the branches that have rows on screen (always the root).
  createEffect(
    on(
      () => props.progress,
      (p) => {
        if (!p) return;
        const active = p.phase === "scanning" || p.phase === "finishing" || p.phase === "paused";
        const now = Date.now();
        const ended = p.phase === "complete" || p.phase === "stopped";
        if (!(ended || (active && now - lastRefresh > REFRESH_MS))) return;
        lastRefresh = now;
        refreshVisible();
      },
    ),
  );

  // A write changed the index: counts and children on screen are refetched.
  createEffect(
    on(
      () => props.version,
      (_v, prev) => {
        if (prev !== undefined) refreshVisible();
      },
    ),
  );

  onMount(() => {
    const close = () => setMenu(null);
    document.addEventListener("click", close);
    document.addEventListener("keydown", close);
    onCleanup(() => {
      document.removeEventListener("click", close);
      document.removeEventListener("keydown", close);
    });
  });

  const exact = () => root()?.exact ?? false;
  const approx = () => (exact() ? "" : "≥ ");
  /// A node's own counts are exact once a scan saw everything under it.
  const nodeExact = (n: TreeNode) => n.scannedAt !== null && n.scan !== "running";
  const freshness = (n: TreeNode) =>
    n.scan === "running"
      ? "being scanned"
      : n.scan === "queued"
        ? "scan queued"
        : n.scannedAt !== null
          ? `scanned ${ago(n.scannedAt)}`
          : "not scanned: the count is a lower bound";
  /// The scan in progress is the scan of exactly this node.
  const scanningHere = (n: TreeNode) => {
    const p = props.progress;
    const live = p && (p.phase === "scanning" || p.phase === "starting" || p.phase === "paused" || p.phase === "finishing");
    return !!live && !!p.subtree && keyId(p.subtree) === keyId({ text: n.prefix, raw: n.prefixRaw });
  };
  const queuedHere = (n: TreeNode) => n.scan === "queued" && asked[branchKey(n.prefix, n.prefixRaw)] === "wanted";
  const pct = (x: number) => `${(x * 100).toFixed(x >= 0.995 ? 0 : 1)} %`;

  const foldOf = (r: { branch: string; fold: number }) => (folds[r.branch] as FoldInfo).folds[r.fold];

  // One virtual item: its flat row and, for a node row, the node behind it.
  const row = (item: VirtualItem) => flat()[item.index];
  const node = (item: VirtualItem) => {
    const r = row(item);
    return r && r.kind === "node" ? branches[r.branch]?.rows[r.index] : undefined;
  };
  const isSelected = (item: VirtualItem) => {
    const n = node(item);
    return !!n && sameKey(n.key, props.selected);
  };

  /// What a fold row shows. The view belongs to the row it was made for.
  const foldView = (r: FlatRow) => {
    const fr = r as { branch: string; fold: number };
    const f = () => foldOf(fr);
    const info = () => folds[fr.branch] as FoldInfo;
    return {
      toggle: () => setOpen(subKey(fr.branch, `f${fr.fold}`), !foldOpen(fr.branch, fr.fold)),
      open: () => foldOpen(fr.branch, fr.fold),
      why: () => `Why folded? ${info().evidence}`,
      label: () => f().label,
      range: () => (f().range ? `${fmtCount(f().range![0])} … ${fmtCount(f().range![1])}` : ""),
      samples: () => (f().samples.length ? `  e.g. ${f().samples.join(", ")}` : ""),
      shareTitle: () => `${pct(f().share)} of ${fmtCount(info().distinct)} distinct children`,
      members: () => fmtCount(f().members),
    };
  };

  /// One line of the structure beneath a fold.
  const summaryView = (r: FlatRow) => {
    const sr = r as { branch: string; fold: number; index: number };
    const e = () => foldOf(sr).summary[sr.index];
    return {
      title: () => `${fmtCount(e().parents)} of ${fmtCount(foldOf(sr).members)} ids have this child; ${fmtCount(e().keys)} keys`,
      hasChildren: () => e().leaves !== e().parents,
      segment: () => e().segment,
      keys: () => fmtCount(e().keys),
      coverage: () => pct(e().coverage),
    };
  };

  /// One bucket of a fold: a range of ids that expands into its members.
  const bucketView = (r: FlatRow) => {
    const br = r as { branch: string; fold: number; index: number };
    const b = () => foldOf(br).buckets[br.index];
    const key = () => subKey(br.branch, `f${br.fold}|b${br.index}`);
    const filter = (): ChildFilter =>
      b().segNumMin !== null
        ? { segNumMin: b().segNumMin!, segNumMax: b().segNumMax! }
        : { segMin: b().segMin!, segMinRaw: b().segMinRaw, segMax: b().segMax!, segMaxRaw: b().segMaxRaw };
    return {
      toggle: () => toggleSub(br.branch, `f${br.fold}|b${br.index}`, filter(), b().segNumMin !== null ? "numeric" : "name"),
      open: () => !!branches[key()]?.expanded,
      label: () => b().label,
      count: () => fmtCount(b().count),
    };
  };

  /// "Show as list": every member of a fold as plain rows.
  const listView = (r: FlatRow) => {
    const lr = r as { branch: string; fold: number };
    const f = () => foldOf(lr);
    const key = () => subKey(lr.branch, `f${lr.fold}|list`);
    return {
      toggle: () => toggleSub(lr.branch, `f${lr.fold}|list`, f().class === "star" ? null : { class: f().class }, f().class === "numeric" ? "numeric" : "name"),
      open: () => !!branches[key()]?.expanded,
      members: () => fmtCount(f().members),
    };
  };

  /// The filter box of a fold.
  const filterView = (r: FlatRow) => {
    const qr = r as { branch: string; fold: number };
    const qk = () => subKey(qr.branch, `f${qr.fold}|q`);
    return {
      text: () => queries[qk()]?.text ?? "",
      setText: (text: string) => setQuery(qr.branch, qr.fold, text),
      matches: () => (queries[qk()]?.count !== null && queries[qk()]?.count !== undefined ? `${fmtCount(queries[qk()]!.count!)} match` : ""),
    };
  };

  return {
    menu,
    sort,
    setSort: (value: string) => setSort(value as TreeSort),
    setScrollEl: (el: HTMLDivElement) => (scrollEl = el),
    items: () => virtualizer.getVirtualItems(),
    innerStyle: () => ({ height: `${virtualizer.getTotalSize()}px` }),
    topLevelLabel: () => (root() ? `${fmtCount(root()!.children)} top-level` : "…"),
    delimiterLabel: () => (root()?.delimiter ? `, delimiter ${JSON.stringify(root()!.delimiter)}` : ""),
    requestsTitle: () => `${requests()} requests`,
    keysLabel: () => {
      const r = root();
      if (!r) return "";
      // on demand the index holds what was sampled and scanned so far
      if (onDemand() && r.scannedAt === null) return `${fmtCount(r.keys)}${props.conn.dbsize === null ? "" : ` of ${fmtCount(props.conn.dbsize)}`} keys seen`;
      return `${exact() ? "" : "≥ "}${fmtCount(r.keys)} keys`;
    },
    /// The scan-root action: offered while the index is on demand and no scan runs.
    canScanAll: () => onDemand() && !props.conn.scanning && !props.progress?.phase.match(/^(starting|scanning|paused|finishing)$/),
    scanAll: () => props.onScanAll(),
    /// Counts are lower bounds until the scan has completed.
    approx,
    row,
    rowStyle: (item: VirtualItem) => ({ transform: `translateY(${item.start}px)`, "padding-left": `${4 + (row(item)?.depth ?? 0) * 16}px` }),
    nodeClass: (item: VirtualItem) =>
      `t ${isSelected(item) ? "selected" : ""} ${node(item) ? "" : "placeholder"} ${node(item) && onDemand() && node(item)!.scannedAt === null ? "uncovered" : ""}`,
    nodeApprox: (item: VirtualItem) => (nodeExact(node(item)!) ? "" : "≥ "),
    nodeScan: (item: VirtualItem) => (scanningHere(node(item)!) ? "scanning" : queuedHere(node(item)!) ? "queued" : ""),
    activate: (item: VirtualItem) => {
      const n = node(item);
      if (n) toggle(n);
    },
    openMenu: (e: MouseEvent, item: VirtualItem) => {
      e.preventDefault();
      const n = node(item);
      if (n) setMenu({ x: e.clientX, y: e.clientY, node: n });
    },
    nodeHasChildren: (item: VirtualItem) => node(item)!.children > 0,
    nodeOpen: (item: VirtualItem) => !!branches[branchKey(node(item)!.prefix, node(item)!.prefixRaw)]?.expanded,
    segClass: (item: VirtualItem) => `seg ${node(item)!.leaf ? "leaf" : ""}`,
    nodePrefix: (item: VirtualItem) => node(item)!.prefix,
    segLabel: (item: VirtualItem) => (node(item)!.segment === "" ? "(empty)" : node(item)!.segment),
    segBinary: (item: VirtualItem) => node(item)!.raw !== null,
    alsoKey: (item: VirtualItem) => node(item)!.leaf && node(item)!.children > 0,
    childrenTitle: (item: VirtualItem) => `${fmtCount(node(item)!.children)} distinct children, ${freshness(node(item)!)}`,
    nodeCount: (item: VirtualItem) => fmtCount(node(item)!.count),
    moreLabel: (r: FlatRow) => (branches[r.branch]?.loading ? "Loading…" : `… ${fmtCount(branches[r.branch]?.rows.length ?? 0)} loaded, scroll for more`),
    foldView,
    summaryView,
    bucketView,
    listView,
    filterView,
    menuStyle: (c: TreeMenu) => ({ left: `${c.x}px`, top: `${c.y}px` }),
    menuTitle: (c: TreeMenu) => c.node.prefix,
    menuCount: (c: TreeMenu) => fmtCount(c.node.count),
    keepMenu: (e: MouseEvent) => e.stopPropagation(),
    /// The search box takes text, so a prefix with other bytes cannot be searched from here.
    canSearchPrefix: (c: TreeMenu) => c.node.prefixRaw === null,
    searchPrefix: (c: TreeMenu) => {
      props.onSearch(`${globEscape(c.node.prefix)}*`);
      setMenu(null);
    },
    scanLabel: (c: TreeMenu) => (c.node.scannedAt === null ? "Scan this subtree" : "Rescan this subtree"),
    scanTitle: (c: TreeMenu) => `The server walks all its keys; only the ones under this prefix cross the network. Now ${freshness(c.node)}.`,
    scanNode: (c: TreeMenu) => {
      void scanSubtree(c.node, true);
      setMenu(null);
    },
    canOpenKey: (c: TreeMenu) => c.node.key !== null,
    openKey: (c: TreeMenu) => {
      if (c.node.key) props.onSelect(c.node.key);
      setMenu(null);
    },
    /// Every key under exactly this prefix: the backend builds the pattern
    /// from the bytes, so glob characters in the prefix match only themselves.
    deletePrefix: (c: TreeMenu) => {
      props.onDeletePattern({ query: `${c.node.prefix}*`, prefix: { text: c.node.prefix, raw: c.node.prefixRaw } });
      setMenu(null);
    },
  };
}
