import { createRoot } from "solid-js";
import { createStore } from "solid-js/store";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SearchPage, SearchRow, VerifyReport } from "./api";
import { createSearchResultsModel, type SearchResultsProps } from "./searchResultsModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection } from "./test/fixtures";
import { scrollBox, scrollTo } from "./test/scroll";

const ROW = 28;
const DEBOUNCE = 150;

const row = (name: string): SearchRow => ({ name, raw: null, ttl: null, mem: null });
const names = (n: number, from = 0) => Array.from({ length: n }, (_, i) => row(`k${String(from + i).padStart(5, "0")}`));
const page = (over: Partial<SearchPage> = {}): SearchPage => ({
  mode: "substring",
  rows: [],
  done: true,
  cancelled: false,
  viaFts: false,
  fts: false,
  ftsApplicable: true,
  examined: 0,
  tookMs: 12,
  verifiable: true,
  ...over,
});
const report = (over: Partial<VerifyReport> = {}): VerifyReport => ({ calls: 3, examined: 100, matched: 2, added: 0, removed: 0, complete: true, truncated: false, tookMs: 40, ...over });

type Search = (id: number, query: string, after: string | null, afterRaw: string | null, fresh: boolean) => Promise<SearchPage>;

function model(search: Search, impl: Parameters<typeof fakeApi>[0] = {}, over: Partial<SearchResultsProps> = {}) {
  const api = fakeApi({
    search: vi.fn(search),
    searchCancel: () => Promise.resolve(),
    searchCount: () => Promise.resolve(null),
    keyMeta: (_id, keys) => Promise.resolve(keys.map(() => ({ exists: true, keyType: "string", ttl: null, mem: 64 }))),
    ...impl,
  });
  const el = scrollBox(600);
  const onSelect = vi.fn();
  const onDeletePattern = vi.fn();
  const [props, setProps] = createStore<SearchResultsProps>({ conn: connection({ id: 9 }), query: "", scanning: false, selected: null, onSelect, version: 0, onDeletePattern, ...over });
  const m = createRoot(() => {
    const m = createSearchResultsModel(props);
    m.setScrollEl(el);
    return m;
  });
  /// Type a query and wait until its search has come back.
  const search_ = async (query: string) => {
    setProps("query", query);
    await vi.advanceTimersByTimeAsync(DEBOUNCE);
    await settle();
  };
  return { m, api, el, setProps, onSelect, onDeletePattern, search: search_ };
}

beforeEach(() => void vi.useFakeTimers());

describe("a new query", () => {
  it("searches once, after the typing paused", async () => {
    const { api, setProps } = model(() => Promise.resolve(page()));
    for (const q of ["s", "se", "ses"]) {
      setProps("query", q);
      await vi.advanceTimersByTimeAsync(DEBOUNCE - 1);
    }
    expect(api.search).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1);
    expect(api.search.mock.calls).toEqual([[9, "ses", null, null, true]]);
    expect(api.searchCancel).toHaveBeenCalledTimes(4);
  });

  it("shows the matches and counts them from a complete first window", async () => {
    const { m, api, search } = model(() => Promise.resolve(page({ rows: names(3), mode: "glob" })));
    await search("k*");
    expect(m.items().map((i) => m.rowName(i.index))).toEqual(["k00000", "k00001", "k00002"]);
    expect(m.summary()).toBe("3 matches, 12 ms");
    expect(m.badgeClass()).toBe("badge success");
    expect(api.searchCount).not.toHaveBeenCalled();
    expect(m.canDeleteAll()).toBe(true);
  });

  it("asks the backend for the count when the first window is not all of it", async () => {
    const { m, api, search } = model(() => Promise.resolve(page({ rows: names(200), done: false })), { searchCount: () => Promise.resolve(4321) });
    await search("k");
    expect(api.searchCount).toHaveBeenCalledWith(9, "k");
    expect(m.summary()).toBe("4,321 matches, 12 ms");
  });

  it("says at least so many while the count is unknown", async () => {
    const { m, search } = model(() => Promise.resolve(page({ rows: names(200), done: false })));
    await search("k");
    expect(m.summary()).toBe("≥ 200 matches, 12 ms");
    expect(m.canDeleteAll()).toBe(false);
  });

  it("drops the reply to a query that was replaced meanwhile", async () => {
    const replies = new Map<string, (p: SearchPage) => void>();
    const { m, setProps } = model((_id, query) => new Promise((resolve) => replies.set(query, resolve)));
    setProps("query", "old");
    await vi.advanceTimersByTimeAsync(DEBOUNCE);
    setProps("query", "new");
    await vi.advanceTimersByTimeAsync(DEBOUNCE);

    replies.get("old")!(page({ rows: names(5) }));
    await settle();
    expect(m.items()).toHaveLength(0);

    replies.get("new")!(page({ rows: names(1, 7) }));
    await settle();
    expect(m.items().map((i) => m.rowName(i.index))).toEqual(["k00007"]);
  });

  it("clears the pane and searches nothing for an empty query", async () => {
    const { m, api, search, setProps } = model(() => Promise.resolve(page({ rows: names(3) })));
    await search("k");
    setProps("query", "");
    await vi.advanceTimersByTimeAsync(DEBOUNCE);
    expect(m.items()).toHaveLength(0);
    expect(m.summary()).toBe("");
    expect(api.search).toHaveBeenCalledTimes(1);
  });

  it("shows the error of a failed search", async () => {
    const { m, search } = model(() => Promise.reject("invalid regex"));
    await search("/(/");
    expect(m.error()).toBe("invalid regex");
  });

  it("tells an empty result apart from one that is still loading", async () => {
    const { m, search } = model(() => Promise.resolve(page()));
    expect(m.noMatches()).toBeFalsy();
    await search("nothing");
    expect(m.noMatches()).toBe(true);
    expect(m.summary()).toBe("0 matches, 12 ms");
  });
});

describe("paging", () => {
  const pages: Search = (_id, _query, after) => Promise.resolve(after === null ? page({ rows: names(200), done: false }) : page({ rows: names(50, 200), done: true }));

  it("loads the next window when the end scrolls into view, keyed on the last row", async () => {
    const { m, api, el, search } = model(pages);
    await search("k");
    expect(api.search).toHaveBeenCalledTimes(1);

    // the virtualizer hears the scroll before the model marks it unsettled,
    // so the request may leave on the first scroll event already
    scrollTo(el, 201 * ROW - 600);
    await vi.advanceTimersByTimeAsync(80);
    await settle();
    expect(api.search).toHaveBeenLastCalledWith(9, "k", "k00199", null, false);
    expect(m.rowName(249)).toBe("k00249");
    expect(m.isPlaceholder(250)).toBe(true);
  });

  it("selects a row by its key and marks the selected one", async () => {
    const { m, search, onSelect, setProps } = model(pages);
    await search("k");
    m.select(2);
    expect(onSelect).toHaveBeenCalledWith({ text: "k00002", raw: null });
    setProps("selected", { text: "k00002", raw: null });
    expect([m.isSelected(2), m.isSelected(3)]).toEqual([true, false]);
  });

  it("stops at 10,000 rows and asks for a narrower query", async () => {
    const { m, api, el, search } = model(() => Promise.resolve(page({ rows: names(10_000), done: false })));
    await search("k");
    expect(m.summary()).toBe("≥ 10,000 matches, 12 ms, showing the first 10,000, refine the query");

    scrollTo(el, 10_000 * ROW - 600);
    await vi.advanceTimersByTimeAsync(80);
    await settle();
    expect(m.items().at(-1)!.index).toBe(9_999);
    expect(api.search).toHaveBeenCalledTimes(1);
  });

  it("fetches the metadata of the rows on screen, a moment later", async () => {
    const { m, api, search } = model(pages);
    await search("k");
    expect(api.keyMeta).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(DEBOUNCE);
    await settle();
    const asked = api.keyMeta.mock.calls[0][1];
    expect(asked[0]).toEqual({ text: "k00000", raw: null });
    expect(asked.length).toBeLessThan(200);
    expect([m.rowType(0), m.rowMem(0)]).toEqual(["string", "64 B"]);
  });
});

describe("after the first result", () => {
  it("runs the query again when the index changed", async () => {
    const { api, search, setProps } = model(() => Promise.resolve(page({ rows: names(3) })));
    await search("k");
    setProps("version", 1);
    await settle();
    expect(api.search.mock.calls).toEqual([
      [9, "k", null, null, true],
      [9, "k", null, null, true],
    ]);
  });

  it("offers the trigram index only for a slow search it would speed up", async () => {
    const offer = async (p: Partial<SearchPage>, scanning = false) => {
      const { m, search } = model(() => Promise.resolve(page(p)), {}, { scanning });
      await search("k");
      return !!m.canEnableFts();
    };
    expect(await offer({ tookMs: 501 })).toBe(true);
    expect(await offer({ tookMs: 500 })).toBe(false);
    expect(await offer({ tookMs: 900, ftsApplicable: false })).toBe(false);
    expect(await offer({ tookMs: 900, fts: true })).toBe(false);
    expect(await offer({ tookMs: 900 }, true)).toBe(false);
  });

  it("runs the query again once the trigram index is ready, for its own session only", async () => {
    const { m, api, search } = model(() => Promise.resolve(page({ tookMs: 900 })));
    await search("k");
    api.emit("onFtsProgress", { session: 8, phase: "ready", done: 1, total: 1, error: null });
    api.emit("onFtsProgress", { session: 9, phase: "building", done: 50, total: 200, error: null });
    await settle();
    expect(api.search).toHaveBeenCalledTimes(1);
    expect([m.ftsBusy(), m.ftsPercent(), !!m.canEnableFts()]).toEqual([true, "25 %", false]);

    api.emit("onFtsProgress", { session: 9, phase: "ready", done: 200, total: 200, error: null });
    await settle();
    expect(api.search).toHaveBeenCalledTimes(2);
  });

  it("searches again when the server knew keys the index did not", async () => {
    const { m, api, search } = model(() => Promise.resolve(page({ rows: names(2) })), { searchVerify: () => Promise.resolve(report({ added: 1 })) });
    await search("k");
    await m.runVerify();
    await settle();
    expect(m.verifyNote()).toMatchObject({ label: "Verified", added: "1", removed: "0" });
    expect(api.search).toHaveBeenCalledTimes(2);
  });

  it("leaves the result alone when the server agrees with the index", async () => {
    const { m, api, search } = model(() => Promise.resolve(page({ rows: names(2) })), { searchVerify: () => Promise.resolve(report({ complete: false, truncated: true })) });
    await search("k");
    await m.runVerify();
    expect(m.verifyNote()).toMatchObject({ label: "Partially verified", truncated: ", stopped at the limit" });
    expect(api.search).toHaveBeenCalledTimes(1);
  });

  it("offers neither verification nor the delete while a scan runs", async () => {
    const { m, search } = model(() => Promise.resolve(page({ rows: names(2) })), {}, { scanning: true });
    await search("k");
    expect([!!m.canVerify(), m.canDeleteAll()]).toEqual([false, false]);
  });

  it("hands the typed query to the pattern delete", async () => {
    const { m, search, onDeletePattern } = model(() => Promise.resolve(page({ rows: names(2) })));
    await search("user:*");
    m.deletePattern();
    expect(onDeletePattern).toHaveBeenCalledWith({ query: "user:*", prefix: null });
  });
});
