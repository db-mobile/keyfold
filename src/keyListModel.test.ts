import { createRoot, createSignal } from "solid-js";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Connection, KeyRef, ListWindow, ScanProgress } from "./api";
import { createFlatListModel, createKeyListModel, type FlatListProps, type KeyListProps } from "./keyListModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection, indexInfo, scanProgress } from "./test/fixtures";
import { scrollBox, scrollTo } from "./test/scroll";

const ROW = 28;
const WINDOW = 200;
const TOTAL = 100_000;

const name = (i: number) => `key:${String(i).padStart(6, "0")}`;
/// A backend whose index holds `total` keys named after their position.
const listRows = (total = TOTAL) => (_id: number, offset: number, limit = WINDOW): Promise<ListWindow> =>
  Promise.resolve({
    total,
    exact: true,
    rows: Array.from({ length: Math.max(0, Math.min(limit, total - offset)) }, (_, i) => ({ name: name(offset + i), raw: null, keyType: null, ttl: null, mem: null })),
  });

function flat(conn: Partial<Connection> = {}, rows = listRows()) {
  const api = fakeApi({ listRows: vi.fn(rows), keyMeta: (_id, names) => Promise.resolve(names.map(() => ({ exists: true, keyType: "string", ttl: null, mem: 2048 }))) });
  const el = scrollBox(600);
  const onSelect = vi.fn();
  // signals, not a store: a store would merge a new progress into the old
  // object, and the model reacts to each event being a new one
  const [progress, setProgress] = createSignal<ScanProgress | null>(null);
  const [selected, setSelected] = createSignal<KeyRef | null>(null);
  const [version, setVersion] = createSignal(0);
  const conn_ = connection({ id: 3, index: indexInfo({ keys: TOTAL, status: "complete" }), ...conn });
  const props: FlatListProps = {
    conn: conn_,
    get progress() {
      return progress();
    },
    get selected() {
      return selected();
    },
    onSelect,
    get version() {
      return version();
    },
  };
  const m = createRoot(() => {
    const m = createFlatListModel(props);
    m.setScrollEl(el);
    return m;
  });
  /// Scroll so that row `i` is the first one in view, and let the scroll settle.
  const show = async (i: number) => {
    scrollTo(el, i * ROW);
    await vi.advanceTimersByTimeAsync(80);
    await settle();
  };
  const windowsAsked = () => api.listRows.mock.calls.map(([, offset]) => offset / WINDOW);
  return { m, api, el, setProgress, setSelected, setVersion, onSelect, show, windowsAsked };
}

beforeEach(() => void vi.useFakeTimers());

describe("the flat list", () => {
  it("asks for one window of 200 rows on opening, never for the keyspace", async () => {
    const { m, api } = flat();
    await settle();
    expect(api.listRows.mock.calls).toEqual([[3, 0, 200]]);
    expect(m.rowName(0)).toBe(name(0));
    expect(m.rowName(199)).toBe(name(199));
    expect(m.isPlaceholder(200)).toBe(true);
    expect(m.rowName(200)).toBe("…");
    expect(m.totalText()).toBe("100,000");
    expect(m.approxMark()).toBe("");
  });

  it("requests nothing when the index is empty", async () => {
    const { api } = flat({ index: indexInfo({ keys: 0 }) });
    await settle();
    expect(api.listRows).not.toHaveBeenCalled();
  });

  it("maps a row to its window and the position in it", async () => {
    const { m, api, show } = flat();
    await show(50_100);
    expect(api.listRows).toHaveBeenLastCalledWith(3, 50_000, 200);
    expect(m.rowName(50_100)).toBe(name(50_100));
    expect(m.rowName(50_199)).toBe(name(50_199));
    expect(m.isPlaceholder(50_200)).toBe(true);
  });

  it("requests both windows when the view spans a boundary", async () => {
    const { windowsAsked, show } = flat();
    await show(190);
    expect(windowsAsked()).toEqual([0, 1]);
  });

  it("does not request a window twice", async () => {
    const { api, show } = flat();
    await show(1_100);
    await show(1_120);
    await show(1_100);
    expect(api.listRows).toHaveBeenCalledTimes(2);
  });

  it("keeps at most 32 windows and lets the oldest go", async () => {
    const { m, show, windowsAsked } = flat();
    await settle();
    for (let w = 1; w <= 33; w++) await show(w * WINDOW + 100);
    expect(windowsAsked()).toHaveLength(34);
    expect(m.cacheTitle()).toBe("32 windows cached, 34 window requests");
    expect(m.isPlaceholder(0)).toBe(true);
    expect(m.isPlaceholder(1 * WINDOW + 100)).toBe(true);
    expect(m.rowName(2 * WINDOW + 100)).toBe(name(2 * WINDOW + 100));
    expect(m.rowName(33 * WINDOW + 100)).toBe(name(33 * WINDOW + 100));
  });

  it("fetches a window again after it was let go", async () => {
    const { api, show } = flat();
    await settle();
    for (let w = 1; w <= 33; w++) await show(w * WINDOW + 100);
    await show(100);
    expect(api.listRows).toHaveBeenCalledTimes(35);
    expect(api.listRows).toHaveBeenLastCalledWith(3, 0, 200);
  });

  it("selects a loaded row by its key and ignores a placeholder", async () => {
    const { m, onSelect, setSelected } = flat();
    await settle();
    m.select(5);
    m.select(5_000);
    expect(onSelect.mock.calls).toEqual([[{ text: name(5), raw: null }]]);
    setSelected({ text: name(5), raw: null });
    expect([m.isSelected(5), m.isSelected(6)]).toEqual([true, false]);
  });

  it("fetches the metadata of the rows on screen only, a moment after the scroll stopped", async () => {
    const { m, api } = flat();
    await settle();
    expect(api.keyMeta).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(150);
    await settle();
    const asked = api.keyMeta.mock.calls[0][1];
    expect(asked[0]).toEqual({ text: name(0), raw: null });
    expect(asked.length).toBeLessThan(WINDOW);
    expect([m.rowType(0), m.rowMem(0)]).toEqual(["string", "2.0 KB"]);
  });
});

describe("changes behind the list", () => {
  it("refetches what is on screen after a write and drops the other windows", async () => {
    const { m, api, show, setVersion } = flat();
    await settle();
    await show(1_100);
    api.listRows.mockClear();
    setVersion(1);
    await settle();
    expect(api.listRows.mock.calls).toEqual([[3, 1_000, 200]]);
    expect(m.isPlaceholder(0)).toBe(true);
    expect(m.rowName(1_100)).toBe(name(1_100));
  });

  it("refreshes the rows on screen during a scan at most every 2 s", async () => {
    const { api, setProgress } = flat();
    await settle();
    api.listRows.mockClear();

    vi.setSystemTime(10_000);
    setProgress(scanProgress({ phase: "scanning", indexKeys: TOTAL, scanned: 1 }));
    await settle();
    expect(api.listRows).toHaveBeenCalledTimes(1);

    vi.setSystemTime(11_900);
    setProgress(scanProgress({ phase: "scanning", indexKeys: TOTAL, scanned: 2 }));
    await settle();
    expect(api.listRows).toHaveBeenCalledTimes(1);

    vi.setSystemTime(12_001);
    setProgress(scanProgress({ phase: "scanning", indexKeys: TOTAL, scanned: 3 }));
    await settle();
    expect(api.listRows).toHaveBeenCalledTimes(2);
  });

  it("marks the total approximate until the scan completed, and refreshes at the end at once", async () => {
    const { m, api, setProgress } = flat({}, (id, offset, limit) => listRows()(id, offset, limit).then((w) => ({ ...w, exact: false })));
    await settle();
    vi.setSystemTime(10_000);
    setProgress(scanProgress({ phase: "scanning", indexKeys: TOTAL }));
    await settle();
    expect(m.approxMark()).toBe("≥ ");

    api.listRows.mockImplementation(listRows());
    vi.setSystemTime(10_100);
    setProgress(scanProgress({ phase: "complete", indexKeys: TOTAL }));
    await settle();
    expect(api.listRows).toHaveBeenCalledTimes(3);
    expect(m.approxMark()).toBe("");
  });

  it("shows the first keys of a first scan as soon as the index has some", async () => {
    const { m, api, setProgress } = flat({ index: indexInfo({ keys: 0 }), scanning: true }, listRows(500));
    await settle();
    vi.setSystemTime(10_000);
    setProgress(scanProgress({ phase: "scanning", indexKeys: 0 }));
    vi.setSystemTime(10_050);
    setProgress(scanProgress({ phase: "scanning", indexKeys: 500 }));
    await settle();
    expect(m.totalText()).toBe("500");
    expect(api.listRows).toHaveBeenCalledWith(3, 0, 200);
    expect(m.rowName(0)).toBe(name(0));
  });
});

describe("the navigation pane", () => {
  function pane(over: Partial<KeyListProps> = {}) {
    const onQuery = vi.fn();
    const onSelect = vi.fn();
    const onView = vi.fn();
    const props: KeyListProps = {
      view: "tree",
      onView,
      tree: null,
      footer: null,
      conn: connection(),
      progress: null,
      selected: null,
      onSelect,
      query: "",
      onQuery,
      version: 0,
      onDeletePattern: vi.fn(),
      onNewKey: vi.fn(),
      ...over,
    };
    const m = createKeyListModel(props);
    const key = (key: string) => m.onSearchKey({ key } as KeyboardEvent);
    return { m, key, onQuery, onSelect, onView };
  }

  it("opens an exact name on Enter", () => {
    const { key, onSelect } = pane({ query: "  user:42:profile " });
    key("Enter");
    expect(onSelect).toHaveBeenCalledWith({ text: "user:42:profile", raw: null });
  });

  it("does not open a glob, a regex or nothing on Enter", () => {
    for (const query of ["user:*", "user:4?", "user:[0-9]", "/user:\\d+/", "   "]) {
      const { key, onSelect } = pane({ query });
      key("Enter");
      expect(onSelect, query).not.toHaveBeenCalled();
    }
  });

  it("clears the search on Escape", () => {
    const { key, onQuery } = pane({ query: "user" });
    key("Escape");
    expect(onQuery).toHaveBeenCalledWith("");
  });

  it("shows the tree or the list, and neither while searching", () => {
    expect(pane({ view: "tree" }).m.treeStyle()).toEqual({ display: "flex" });
    expect(pane({ view: "list" }).m.listVisible()).toBe(true);
    const searching = pane({ view: "list", query: "user" }).m;
    expect([searching.searching(), searching.listVisible(), searching.treeStyle().display]).toEqual([true, false, "none"]);
  });

  it("becomes one list of SCAN pages without an index", () => {
    const { m } = pane({ conn: connection({ indexMode: "none" }), query: "user:*", view: "list" });
    expect([m.noIndex(), m.searching(), m.listVisible()]).toEqual([true, false, false]);
    expect(m.searchPlaceholder()).toBe("Pattern for SCAN MATCH");
  });

  it("counts a scan as running from the progress, or from the connection before the first event", () => {
    expect(pane({ progress: scanProgress({ phase: "paused" }) }).m.scanning()).toBe(true);
    expect(pane({ progress: scanProgress({ phase: "complete" }), conn: connection({ scanning: true }) }).m.scanning()).toBe(false);
    expect(pane({ conn: connection({ scanning: true }) }).m.scanning()).toBe(true);
  });
});
