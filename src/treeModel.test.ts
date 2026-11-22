import { createRoot, createSignal } from "solid-js";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ChildFilter, Connection, Fold, FoldInfo, KeyRef, ScanProgress, TreeAfter, TreeNode, TreeSort } from "./api";
import { createDialogHostModel } from "./dialogModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection, scanProgress } from "./test/fixtures";
import { scrollBox, scrollTo } from "./test/scroll";
import { createTreeModel, type TreeProps } from "./treeModel";

const ROW = 28;
const CLASSES = { uuid: 0, ulid: 0, hex: 0, numeric: 0, date: 0, opaque: 0, email: 0, literal: 0 };

/// A namespace node `parent + segment + ":"`.
const branch = (parent: string, segment: string, over: Partial<TreeNode> = {}): TreeNode => ({
  segment,
  raw: null,
  prefix: `${parent}${segment}:`,
  prefixRaw: null,
  key: null,
  scannedAt: 1,
  scan: "idle",
  leaf: false,
  count: 10,
  segNum: /^\d+$/.test(segment) ? Number(segment) : null,
  children: 3,
  classes: CLASSES,
  ...over,
});
/// A key `parent + segment` without children.
const leaf = (parent: string, segment: string, over: Partial<TreeNode> = {}): TreeNode =>
  branch(parent, segment, { key: { text: `${parent}${segment}`, raw: null }, leaf: true, count: 1, children: 0, ...over });

const fold = (over: Partial<Fold> = {}): Fold => ({
  class: "numeric",
  label: "{id}",
  members: 148,
  share: 0.987,
  range: [1, 148],
  samples: ["1", "2"],
  summary: [],
  summaryDistinct: 0,
  membersWithChildren: 148,
  buckets: [],
  ...over,
});
const foldInfo = (over: Partial<FoldInfo> = {}): FoldInfo => ({ distinct: 150, evidence: "98.7 % of the children are numeric", folded: true, folds: [fold()], literals: [], computedMs: 2, cached: false, ...over });

/// A backend over a tree given as children per prefix. It answers like the
/// real one: windows of 200 after a row, a segment range or a numeric range
/// as filter. Rows are copied, as they would be when they cross the IPC.
function backend(tree: Record<string, TreeNode[]>, folds: Record<string, FoldInfo> = {}) {
  const treeChildren = vi.fn((_id: number, prefix: string, raw: string | null, after: TreeAfter | null, _sort: TreeSort, filter: ChildFilter | null = null) => {
    let all = tree[raw ? `raw:${raw}` : prefix] ?? [];
    if (filter?.segMin !== undefined) all = all.filter((n) => n.segment >= filter.segMin! && n.segment <= filter.segMax!);
    if (filter?.segNumMin !== undefined) all = all.filter((n) => n.segNum !== null && n.segNum >= filter.segNumMin! && n.segNum <= filter.segNumMax!);
    const start = after ? all.findIndex((n) => n.segment === after.segment) + 1 : 0;
    return Promise.resolve({ nodes: all.slice(start, start + 200).map((n) => ({ ...n })), exact: true });
  });
  const treeRoot = vi.fn(() => Promise.resolve({ children: tree[""].length, classes: CLASSES, delimiter: ":", keys: 1234, exact: true, scannedAt: 1 }));
  const treeFold = vi.fn((_id: number, prefix: string) => (folds[prefix] ? Promise.resolve(folds[prefix]) : Promise.reject(`no fold for ${prefix}`)));
  return { treeChildren, treeRoot, treeFold };
}

async function tree(nodes: Record<string, TreeNode[]>, opts: { folds?: Record<string, FoldInfo>; conn?: Partial<Connection>; height?: number; impl?: Parameters<typeof fakeApi>[0] } = {}) {
  const api = fakeApi({ ...backend(nodes, opts.folds), ...opts.impl });
  const el = scrollBox(opts.height ?? 2800);
  const [progress, setProgress] = createSignal<ScanProgress | null>(null);
  const [selected, setSelected] = createSignal<KeyRef | null>(null);
  const [version, setVersion] = createSignal(0);
  const calls = { onSelect: vi.fn(), onSearch: vi.fn(), onDeletePattern: vi.fn(), onScanAll: vi.fn(), onRefresh: vi.fn() };
  const props: TreeProps = {
    conn: connection({ id: 4, ...opts.conn }),
    get progress() {
      return progress();
    },
    get selected() {
      return selected();
    },
    get version() {
      return version();
    },
    ...calls,
  };
  const m = createRoot(() => {
    const m = createTreeModel(props);
    m.setScrollEl(el);
    return m;
  });
  await settle();

  /// The rows on screen, one line each, indented by depth.
  const rows = () =>
    m.items().map((item) => {
      const r = m.row(item);
      const pad = "  ".repeat(r.depth);
      switch (r.kind) {
        case "node":
          return `${pad}${m.nodeHasChildren(item) ? (m.nodeOpen(item) ? "v " : "> ") : ""}${m.segLabel(item)} ${m.nodeApprox(item)}${m.nodeCount(item)}`;
        case "fold":
          return `${pad}[fold ${m.foldView(r).open() ? "v" : ">"}] ${m.foldView(r).label()} ${m.foldView(r).members()}`;
        case "summary":
          return `${pad}[summary] ${m.summaryView(r).segment()} ${m.summaryView(r).coverage()}`;
        case "bucket":
          return `${pad}[bucket ${m.bucketView(r).open() ? "v" : ">"}] ${m.bucketView(r).label()} ${m.bucketView(r).count()}`;
        case "list":
          return `${pad}[list ${m.listView(r).open() ? "v" : ">"}] ${m.listView(r).members()}`;
        case "filter":
          return `${pad}[filter] ${m.filterView(r).text()} ${m.filterView(r).matches()}`.trimEnd();
        case "more":
          return `${pad}[more] ${m.moreLabel(r)}`;
        case "folding":
          return `${pad}[folding]`;
      }
    });
  /// The virtual item of the first row that reads like `text`.
  const item = (text: string) => {
    const i = rows().findIndex((r) => r.trim().startsWith(text) || r.trim().replace(/^[v>] /, "").startsWith(text));
    if (i < 0) throw new Error(`no row "${text}" among\n${rows().join("\n")}`);
    return m.items()[i];
  };
  const click = async (text: string) => {
    m.activate(item(text));
    await settle();
  };
  const row = (text: string) => m.row(item(text));
  const childCalls = () => api.treeChildren.mock.calls.map(([, prefix, raw, after, sort, filter]) => ({ prefix, raw, after: after?.segment ?? null, sort, filter }));
  return { m, api, el, rows, item, row, click, childCalls, setProgress, setSelected, setVersion, ...calls };
}

const small = () => ({
  "": [branch("", "user", { count: 30, children: 3 }), branch("", "order", { count: 5, children: 2 }), leaf("", "config")],
  "user:": [branch("user:", "1"), branch("user:", "2"), leaf("user:", "count")],
  "user:1:": [leaf("user:1:", "profile"), leaf("user:1:", "cart")],
});

beforeEach(() => void vi.useFakeTimers());

describe("the root", () => {
  it("loads the root and its first window, sorted by count", async () => {
    const { m, api, rows, childCalls } = await tree(small());
    expect(api.treeRoot).toHaveBeenCalledWith(4);
    expect(childCalls()).toEqual([{ prefix: "", raw: null, after: null, sort: "count", filter: null }]);
    expect(rows()).toEqual(["> user 30", "> order 5", "config 1"]);
    expect([m.topLevelLabel(), m.delimiterLabel(), m.keysLabel()]).toEqual(["3 top-level", ', delimiter ":"', "1,234 keys"]);
  });

  it("marks the count of a subtree no scan has covered as a lower bound", async () => {
    const { m, rows, item } = await tree({ "": [branch("", "user", { scannedAt: null }), branch("", "order", { scan: "running" })] });
    expect(rows()).toEqual(["> user ≥ 10", "> order ≥ 10"]);
    expect(m.childrenTitle(item("user"))).toBe("3 distinct children, not scanned: the count is a lower bound");
    expect(m.childrenTitle(item("order"))).toBe("3 distinct children, being scanned");
  });
});

describe("expanding", () => {
  it("loads a node's children once, and keeps them when it is collapsed", async () => {
    const { api, rows, click, childCalls } = await tree(small());
    await click("user");
    expect(childCalls().at(-1)).toEqual({ prefix: "user:", raw: null, after: null, sort: "count", filter: null });
    expect(rows()).toEqual(["v user 30", "  > 1 10", "  > 2 10", "  count 1", "> order 5", "config 1"]);

    await click("user");
    expect(rows()).toEqual(["> user 30", "> order 5", "config 1"]);
    await click("user");
    expect(rows()).toHaveLength(6);
    expect(api.treeChildren).toHaveBeenCalledTimes(2);
    expect(api.treeFold).not.toHaveBeenCalled();
  });

  it("keeps a nested level open under a collapsed and reopened parent", async () => {
    const { rows, click } = await tree(small());
    await click("user");
    await click("1");
    await click("user");
    await click("user");
    expect(rows()).toEqual(["v user 30", "  v 1 10", "    profile 1", "    cart 1", "  > 2 10", "  count 1", "> order 5", "config 1"]);
  });

  it("selects a key and does not expand what has no children", async () => {
    const { api, click, onSelect, setSelected, m, item } = await tree(small());
    await click("config");
    expect(onSelect).toHaveBeenCalledWith({ text: "config", raw: null });
    expect(api.treeChildren).toHaveBeenCalledTimes(1);
    setSelected({ text: "config", raw: null });
    expect(m.nodeClass(item("config"))).toContain("selected");
    expect(m.nodeClass(item("order"))).not.toContain("selected");
  });

  it("selects and expands a key that is also a namespace", async () => {
    const both = branch("", "user", { key: { text: "user", raw: null }, leaf: true });
    const { m, item, click, onSelect, rows } = await tree({ "": [both], "user:": [leaf("user:", "a")] });
    expect(m.alsoKey(item("user"))).toBe(true);
    await click("user");
    expect(onSelect).toHaveBeenCalledWith({ text: "user", raw: null });
    expect(rows()).toEqual(["v user 10", "  a 1"]);
  });

  it("asks for the children of a binary prefix by its bytes", async () => {
    const binary = branch("", "\\xff", { raw: "ff", prefix: "\\xff:", prefixRaw: "ff3a" });
    const { m, item, click, childCalls, rows } = await tree({ "": [binary], "raw:ff3a": [leaf("\\xff:", "a")] });
    expect(m.segBinary(item("\\xff"))).toBe(true);
    await click("\\xff");
    expect(childCalls().at(-1)).toMatchObject({ prefix: "\\xff:", raw: "ff3a" });
    expect(rows()).toEqual(["v \\xff 10", "  a 1"]);
  });

  it("loads the next window when the end of a long level scrolls into view", async () => {
    const many = Array.from({ length: 250 }, (_, i) => leaf("", `k${String(i).padStart(3, "0")}`));
    const { m, el, rows, childCalls } = await tree({ "": many }, { height: 600 });
    expect(childCalls()).toHaveLength(1);
    scrollTo(el, 201 * ROW - 600);
    await vi.advanceTimersByTimeAsync(80);
    await settle();
    expect(childCalls().at(-1)).toMatchObject({ prefix: "", after: "k199" });
    scrollTo(el, 250 * ROW - 600);
    await vi.advanceTimersByTimeAsync(80);
    await settle();
    expect(rows().at(-1)).toBe("k249 1");
    expect(m.requestsTitle()).toBe("2 requests");
  });
});

describe("folding", () => {
  const big = (literals: TreeNode[] = [leaf("user:", "count")], f: Partial<Fold> = {}) => ({
    nodes: {
      "": [branch("", "user", { count: 9000, children: 150 })],
      "user:": [...Array.from({ length: 148 }, (_, i) => branch("user:", String(i + 1))), ...literals],
    },
    folds: { "user:": foldInfo({ literals, folds: [fold(f)] }) },
  });

  it("asks the backend how a level of 100 children or more folds, not for its rows", async () => {
    const { nodes, folds } = big();
    const { api, rows, click, m, row } = await tree(nodes, { folds });
    await click("user");
    expect(api.treeFold).toHaveBeenCalledWith(4, "user:", null);
    expect(api.treeChildren).toHaveBeenCalledTimes(1);
    expect(rows()).toEqual(["v user 9,000", "  [fold >] {id} 148", "  count 1"]);
    const view = m.foldView(row("[fold"));
    expect([view.why(), view.range(), view.samples(), view.shareTitle()]).toEqual([
      "Why folded? 98.7 % of the children are numeric",
      "1 … 148",
      "  e.g. 1, 2",
      "98.7 % of 150 distinct children",
    ]);
  });

  it("shows a placeholder while the decision is on its way", async () => {
    const { nodes } = big();
    let decide: (f: FoldInfo) => void = () => {};
    const { rows, click } = await tree(nodes, { impl: { treeFold: () => new Promise<FoldInfo>((resolve) => (decide = resolve)) } });
    await click("user");
    expect(rows()).toEqual(["v user 9,000", "  [folding]"]);
    decide(foldInfo());
    await settle();
    expect(rows()).toEqual(["v user 9,000", "  [fold >] {id} 148"]);
  });

  it("loads the level as plain rows when the backend does not fold it, or cannot say", async () => {
    const { nodes } = big();
    const unfolded = await tree(nodes, { folds: { "user:": foldInfo({ folded: false, folds: [] }) } });
    await unfolded.click("user");
    expect(unfolded.childCalls().at(-1)).toMatchObject({ prefix: "user:", after: null });
    expect(unfolded.rows().slice(0, 3)).toEqual(["v user 9,000", "  > 1 10", "  > 2 10"]);

    const failed = await tree(nodes);
    vi.spyOn(console, "error").mockImplementation(() => {});
    await failed.click("user");
    expect(failed.rows().slice(0, 3)).toEqual(["v user 9,000", "  > 1 10", "  > 2 10"]);
  });

  it("opens a fold into its structure, its buckets, the list and the filter box", async () => {
    const summary = Array.from({ length: 14 }, (_, i) => ({ segment: `s${i}`, raw: null, parents: 148 - i, keys: 300, leaves: 148 - i, coverage: (148 - i) / 148 }));
    const bucket = (label: string, min: number, max: number) => ({ label, segNumMin: min, segNumMax: max, segMin: null, segMinRaw: null, segMax: null, segMaxRaw: null, count: max - min + 1 });
    const { nodes, folds } = big([], { summary, buckets: [bucket("1 … 100", 1, 100), bucket("101 … 148", 101, 148)] });
    const { m, rows, row, click, childCalls } = await tree(nodes, { folds });
    await click("user");
    m.foldView(row("[fold")).toggle();
    const open = rows();
    expect(open.slice(0, 3)).toEqual(["v user 9,000", "  [fold v] {id} 148", "    [summary] s0 100 %"]);
    expect(open.filter((r) => r.includes("[summary]"))).toHaveLength(12);
    expect(open.slice(-4)).toEqual(["    [bucket >] 1 … 100 100", "    [bucket >] 101 … 148 48", "    [list >] 148", "    [filter]"]);

    m.bucketView(row("[bucket >] 101")).toggle();
    await settle();
    expect(childCalls().at(-1)).toEqual({ prefix: "user:", raw: null, after: null, sort: "numeric", filter: { segNumMin: 101, segNumMax: 148 } });
    expect(rows().filter((r) => r.startsWith("      ")).length).toBe(48);
    expect(rows()).toContain("      > 101 10");

    m.listView(row("[list")).toggle();
    await settle();
    expect(childCalls().at(-1)).toEqual({ prefix: "user:", raw: null, after: null, sort: "numeric", filter: { class: "numeric" } });
  });

  it("filters the members of a fold by what they start with and counts the matches", async () => {
    const { nodes, folds } = big();
    const { m, api, rows, row, click, childCalls } = await tree(nodes, { folds, impl: { treeCount: vi.fn(() => Promise.resolve(11)) } });
    await click("user");
    m.foldView(row("[fold")).toggle();

    m.filterView(row("[filter]")).setText("14");
    await settle();
    const filter = { segMin: "14", segMax: "14￿" };
    expect(childCalls().at(-1)).toEqual({ prefix: "user:", raw: null, after: null, sort: "count", filter });
    expect(api.treeCount).toHaveBeenCalledWith(4, "user:", null, filter);
    expect(rows().filter((r) => r.startsWith("      "))).toEqual(["      > 14 10", "      > 140 10", "      > 141 10", "      > 142 10", "      > 143 10", "      > 144 10", "      > 145 10", "      > 146 10", "      > 147 10", "      > 148 10"]);
    expect(rows()).toContain("    [filter] 14 11 match");

    m.filterView(row("[filter]")).setText("");
    await settle();
    expect(rows().at(-2)).toBe("    [filter]");
    expect(rows().some((r) => r.startsWith("      "))).toBe(false);
  });

  it("drops the match count of a filter text that was replaced", async () => {
    const { nodes, folds } = big();
    const counts: ((n: number) => void)[] = [];
    const { m, rows, row, click } = await tree(nodes, { folds, impl: { treeCount: () => new Promise<number>((resolve) => counts.push(resolve)) } });
    await click("user");
    m.foldView(row("[fold")).toggle();
    m.filterView(row("[filter]")).setText("1");
    m.filterView(row("[filter]")).setText("14");
    counts[0](59);
    await settle();
    expect(rows()).toContain("    [filter] 14");
    counts[1](11);
    await settle();
    expect(rows()).toContain("    [filter] 14 11 match");
  });
});

describe("a folded level behind which the index changed", () => {
  it("keeps its literal rows and does not list the folded ids beside the fold", async () => {
    const literals = [leaf("user:", "count")];
    const nodes = {
      "": [branch("", "user", { count: 9000, children: 150 })],
      "user:": [...Array.from({ length: 148 }, (_, i) => branch("user:", String(i + 1))), ...literals],
    };
    const { rows, click, setVersion, setProgress } = await tree(nodes, { folds: { "user:": foldInfo({ literals }) } });
    await click("user");
    const folded = ["v user 9,000", "  [fold >] {id} 148", "  count 1"];
    expect(rows()).toEqual(folded);

    setVersion(1);
    await settle();
    expect(rows()).toEqual(folded);

    setProgress(scanProgress({ phase: "complete" }));
    await settle();
    expect(rows()).toEqual(folded);
  });
});

describe("a folded level that shrank", () => {
  it("shows all its children again after it was collapsed and reopened", async () => {
    const nodes: Record<string, TreeNode[]> = {
      "": [branch("", "user", { count: 9000, children: 150 })],
      "user:": [branch("user:", "1"), branch("user:", "2"), leaf("user:", "count")],
    };
    const { rows, click, setVersion } = await tree(nodes, { folds: { "user:": foldInfo({ literals: [leaf("user:", "count")] }) } });
    await click("user");
    expect(rows()).toEqual(["v user 9,000", "  [fold >] {id} 148", "  count 1"]);

    // most ids were deleted: the level is below the fold threshold now
    nodes[""][0] = { ...nodes[""][0], count: 21, children: 3 };
    setVersion(1);
    await settle();
    await click("user");
    await click("user");
    expect(rows()).toEqual(["v user 21", "  > 1 10", "  > 2 10", "  count 1"]);
  });
});

describe("sorting", () => {
  it("reloads the open levels in the new order and leaves closed ones for later", async () => {
    const { m, click, childCalls, api } = await tree(small());
    await click("user");
    await click("1");
    await click("1");
    api.treeChildren.mockClear();
    m.setSort("name");
    await settle();
    expect(childCalls()).toEqual([
      { prefix: "", raw: null, after: null, sort: "name", filter: null },
      { prefix: "user:", raw: null, after: null, sort: "name", filter: null },
    ]);
    await click("1");
    expect(childCalls().at(-1)).toMatchObject({ prefix: "user:1:", sort: "name" });
  });
});

describe("changes behind the tree", () => {
  it("patches counts in place after a write and keeps what is open", async () => {
    const nodes = small();
    const { rows, click, setVersion, api } = await tree(nodes);
    await click("user");
    nodes[""][0] = { ...nodes[""][0], count: 31, children: 4 };
    nodes["user:"][0] = { ...nodes["user:"][0], count: 11 };
    api.treeRoot.mockClear();
    setVersion(1);
    await settle();
    expect(api.treeRoot).toHaveBeenCalledTimes(1);
    expect(rows()).toEqual(["v user 31", "  > 1 11", "  > 2 10", "  count 1", "> order 5", "config 1"]);
  });

  it("replaces a level whose rows changed", async () => {
    const nodes = small();
    const { rows, click, setVersion } = await tree(nodes);
    await click("user");
    nodes["user:"] = [branch("user:", "1"), branch("user:", "3"), leaf("user:", "count"), leaf("user:", "total")];
    setVersion(1);
    await settle();
    expect(rows()).toEqual(["v user 30", "  > 1 10", "  > 3 10", "  count 1", "  total 1", "> order 5", "config 1"]);
  });

  it("refreshes the levels on screen at most once a second while a scan runs, and once more when it ends", async () => {
    const { api, setProgress } = await tree(small());
    api.treeChildren.mockClear();
    vi.setSystemTime(10_000);
    setProgress(scanProgress({ phase: "scanning", scanned: 1 }));
    vi.setSystemTime(10_900);
    setProgress(scanProgress({ phase: "scanning", scanned: 2 }));
    await settle();
    expect(api.treeChildren).toHaveBeenCalledTimes(1);

    vi.setSystemTime(11_001);
    setProgress(scanProgress({ phase: "scanning", scanned: 3 }));
    await settle();
    expect(api.treeChildren).toHaveBeenCalledTimes(2);

    vi.setSystemTime(11_002);
    setProgress(scanProgress({ phase: "complete" }));
    await settle();
    expect(api.treeChildren).toHaveBeenCalledTimes(3);
  });
});

describe("an on-demand index", () => {
  const partial = () => ({ "": [branch("", "user", { scannedAt: null }), branch("", "order")], "user:": [leaf("user:", "a")], "order:": [leaf("order:", "b")] });
  const scanSubtree = () => vi.fn(() => Promise.resolve("started" as const));

  it("scans a subtree the index knows only in part when it is opened, once", async () => {
    const impl = { scanSubtree: scanSubtree() };
    const { api, click, onRefresh, m, item } = await tree(partial(), { conn: { indexMode: "onDemand" }, impl });
    expect(m.nodeClass(item("user"))).toContain("uncovered");
    await click("user");
    expect(api.scanSubtree).toHaveBeenCalledWith(4, { text: "user:", raw: null }, true);
    expect(onRefresh).toHaveBeenCalledTimes(1);
    await click("user");
    await click("user");
    expect(api.scanSubtree).toHaveBeenCalledTimes(1);
  });

  it("does not scan a covered subtree, nor anything with a full index", async () => {
    const covered = await tree(partial(), { conn: { indexMode: "onDemand" }, impl: { scanSubtree: scanSubtree() } });
    await covered.click("order");
    expect(covered.api.scanSubtree).not.toHaveBeenCalled();

    const full = await tree(partial(), { impl: { scanSubtree: scanSubtree() } });
    await full.click("user");
    expect(full.api.scanSubtree).not.toHaveBeenCalled();
  });

  it("asks first on prod, takes a no for the session, and asks again from the menu", async () => {
    const host = createRoot(() => createDialogHostModel());
    const { api, m, item, click } = await tree(partial(), { conn: { indexMode: "onDemand", tag: "prod", dbsize: 5_000_000 }, impl: { scanSubtree: scanSubtree() } });
    await click("user");
    expect(host.asked()?.title).toBe("Scan user:*?");
    expect(String(host.asked()?.body)).toContain("5,000,000 keys");
    host.cancel();
    await settle();
    await click("user");
    await click("user");
    expect(host.asked()).toBeNull();
    expect(api.scanSubtree).not.toHaveBeenCalled();

    m.openMenu({ preventDefault: () => {}, clientX: 10, clientY: 20 } as MouseEvent, item("user"));
    expect(m.scanLabel(m.menu()!)).toBe("Scan this subtree");
    m.scanNode(m.menu()!);
    expect(m.menu()).toBeNull();
    host.confirm();
    await settle();
    expect(api.scanSubtree).toHaveBeenCalledTimes(1);
  });

  it("offers to scan everything only while no scan runs", async () => {
    const { m, setProgress, onScanAll } = await tree(partial(), { conn: { indexMode: "onDemand" } });
    expect(m.canScanAll()).toBe(true);
    m.scanAll();
    expect(onScanAll).toHaveBeenCalledTimes(1);
    setProgress(scanProgress({ phase: "paused" }));
    expect(m.canScanAll()).toBe(false);
    expect((await tree(partial())).m.canScanAll()).toBe(false);
  });
});

describe("the context menu", () => {
  const open = async (node: TreeNode) => {
    const t = await tree({ "": [node] });
    t.m.openMenu({ preventDefault: () => {}, clientX: 10, clientY: 20 } as MouseEvent, t.m.items()[0]);
    return { ...t, menu: t.m.menu()! };
  };

  it("searches under a prefix, with its glob characters taken literally", async () => {
    const { m, menu, onSearch } = await open(branch("", "a*b[1]"));
    expect(m.canSearchPrefix(menu)).toBe(true);
    m.searchPrefix(menu);
    expect(onSearch).toHaveBeenCalledWith("a\\*b\\[1\\]:*");
    expect(m.menu()).toBeNull();
  });

  it("cannot search a binary prefix, but deletes under it by its bytes", async () => {
    const { m, menu, onDeletePattern } = await open(branch("", "\\xff", { raw: "ff", prefix: "\\xff:", prefixRaw: "ff3a" }));
    expect(m.canSearchPrefix(menu)).toBe(false);
    m.deletePrefix(menu);
    expect(onDeletePattern).toHaveBeenCalledWith({ query: "\\xff:*", prefix: { text: "\\xff:", raw: "ff3a" } });
  });

  it("opens the key of a node that is one, and closes on a click elsewhere", async () => {
    const { m, menu, onSelect } = await open(leaf("", "config"));
    expect(m.canOpenKey(menu)).toBe(true);
    m.openKey(menu);
    expect(onSelect).toHaveBeenCalledWith({ text: "config", raw: null });

    const other = await open(branch("", "user"));
    expect(other.m.canOpenKey(other.menu)).toBe(false);
    document.dispatchEvent(new Event("click"));
    expect(other.m.menu()).toBeNull();
  });
});
