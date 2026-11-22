import { createRoot } from "solid-js";
import { createStore } from "solid-js/store";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Connection, KeyRef, KeysPage } from "./api";
import { createScanListModel, type ScanListProps } from "./scanListModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection } from "./test/fixtures";
import { scrollBox } from "./test/scroll";

const keys = (n: number, from = 0): KeyRef[] => Array.from({ length: n }, (_, i) => ({ text: `k${from + i}`, raw: null }));
const page = (over: Partial<KeysPage> = {}): KeysPage => ({ keys: [], done: false, calls: 1, returned: 0, examined: 0, node: null, nodeAt: 0, nodes: 1, mode: null, tookMs: 1, ...over });

type Pager = (id: number, pattern: string | null, restart: boolean) => Promise<KeysPage>;

function model(keysPage: Pager, conn: Partial<Connection> = {}) {
  const api = fakeApi({ keysPage: vi.fn(keysPage), keyMeta: (_id, names) => Promise.resolve(names.map(() => ({ exists: true, keyType: "hash", ttl: 90, mem: null }))) });
  const onSelect = vi.fn();
  const [props, setProps] = createStore<ScanListProps>({ conn: connection({ id: 6, indexMode: "none", ...conn }), query: "", selected: null, onSelect });
  const m = createRoot(() => {
    const m = createScanListModel(props);
    m.setScrollEl(scrollBox(600));
    return m;
  });
  const opened = async () => {
    await vi.advanceTimersByTimeAsync(0);
    await settle();
  };
  return { m, api, setProps, onSelect, opened };
}

beforeEach(() => void vi.useFakeTimers());

describe("the walk", () => {
  it("reads the first page on opening, without a pattern", async () => {
    const { m, api, opened } = model(() => Promise.resolve(page({ keys: keys(3), examined: 30 })));
    await opened();
    expect(api.keysPage.mock.calls).toEqual([[6, null, true]]);
    expect(m.items().map((i) => m.rowName(i.index))).toEqual(["k0", "k1", "k2", ""]);
    expect(m.isAction(3)).toBe(true);
    expect(m.caption()).toBe("3 keys loaded, about 3.0 % walked");
    expect(m.canLoadMore()).toBe(true);
  });

  it("appends the next page only when asked, and not twice at once", async () => {
    const pending: ((p: KeysPage) => void)[] = [];
    const { m, api, opened } = model(() => new Promise((resolve) => pending.push(resolve)));
    await opened();
    pending.shift()!(page({ keys: keys(2) }));
    await settle();

    m.loadMore();
    m.loadMore();
    expect(m.canLoadMore()).toBe(false);
    pending.shift()!(page({ keys: keys(2, 2), done: true }));
    await settle();
    expect(api.keysPage.mock.calls).toEqual([
      [6, null, true],
      [6, null, false],
    ]);
    expect(m.caption()).toBe("4 keys loaded, end of the keyspace");
    expect([m.atEnd(), m.endLabel(), m.canLoadMore()]).toEqual([true, "End of the keyspace", false]);
  });

  it("starts a new walk for a pattern once the typing paused", async () => {
    const { m, api, opened, setProps } = model((_id, pattern) => Promise.resolve(page({ keys: pattern ? keys(1, 100) : keys(3), mode: pattern ? "glob" : null })));
    await opened();
    setProps("query", "u");
    await vi.advanceTimersByTimeAsync(299);
    setProps("query", "user:*");
    await vi.advanceTimersByTimeAsync(299);
    expect(api.keysPage).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    await settle();
    expect(api.keysPage).toHaveBeenLastCalledWith(6, "user:*", true);
    expect(m.items().map((i) => m.rowName(i.index))).toEqual(["k100", ""]);
    expect(m.captionTitle()).toBe("SCAN MATCH (glob), in the server's order");
  });

  it("drops a page of the walk that was replaced", async () => {
    const pending: ((p: KeysPage) => void)[] = [];
    const { m, opened } = model(() => new Promise((resolve) => pending.push(resolve)));
    await opened();
    m.restart();
    pending[0](page({ keys: keys(5) }));
    await settle();
    expect(m.caption()).toBe("0 keys loaded");
    expect(m.loading()).toBe(true);
    pending[1](page({ keys: keys(1, 50) }));
    await settle();
    expect(m.rowName(0)).toBe("k50");
  });

  it("says where a cluster walk stands", async () => {
    const { m, opened } = model(() => Promise.resolve(page({ keys: keys(1), nodes: 3, nodeAt: 1, node: "b:6379" })));
    await opened();
    expect(m.captionTitle()).toBe("SCAN, in the server's order, node 2 of 3 (b:6379)");
  });
});

describe("the action row", () => {
  it("tells an empty database from a pattern without matches", async () => {
    const empty = model(() => Promise.resolve(page({ done: true })));
    await empty.opened();
    expect(empty.m.endLabel()).toBe("The database is empty");

    const none = model(() => Promise.resolve(page({ done: true })));
    none.setProps("query", "nothing*");
    await vi.advanceTimersByTimeAsync(300);
    await settle();
    expect(none.m.endLabel()).toBe("No key matches");
  });

  it("says so when a pattern left a stretch empty with more to walk", async () => {
    const { m, setProps } = model(() => Promise.resolve(page()));
    setProps("query", "rare*");
    await vi.advanceTimersByTimeAsync(300);
    await settle();
    expect(m.loadMoreLabel()).toBe("Nothing in that stretch. Load more");
    expect(m.canLoadMore()).toBe(true);
  });

  it("holds at most 10,000 rows and continues with an empty list", async () => {
    const pages = [page({ keys: keys(10_000) }), page({ keys: keys(4, 10_000), done: true })];
    const { m, api, opened } = model(() => Promise.resolve(pages.shift()!));
    await opened();
    expect([m.canLoadMore(), m.canProceed()]).toEqual([false, true]);
    expect(m.proceedLabel()).toBe("Continue (clears these 10,000 rows)");

    m.proceed();
    await settle();
    expect(api.keysPage).toHaveBeenLastCalledWith(6, null, false);
    expect(m.rowName(0)).toBe("k10000");
    expect(m.isAction(4)).toBe(true);
    expect(m.caption()).toBe("10,004 keys loaded, end of the keyspace");
    expect(m.endLabel()).toBe("End of the keyspace");
  });

  it("offers a retry after an error: the walk again if nothing came, else the next page", async () => {
    const replies: (() => Promise<KeysPage>)[] = [() => Promise.reject("timeout"), () => Promise.resolve(page({ keys: keys(2) })), () => Promise.reject("timeout"), () => Promise.resolve(page({ done: true }))];
    const { m, api, opened } = model(() => replies.shift()!());
    await opened();
    expect([m.error(), m.canRetry(), m.canLoadMore()]).toEqual(["timeout", true, false]);
    m.retry();
    await settle();
    expect(api.keysPage).toHaveBeenLastCalledWith(6, null, true);
    expect(m.error()).toBeNull();

    m.loadMore();
    await settle();
    m.retry();
    await settle();
    expect(api.keysPage).toHaveBeenLastCalledWith(6, null, false);
    expect(m.caption()).toBe("2 keys loaded, end of the keyspace");
  });
});

describe("rows", () => {
  it("selects by key reference, binary names included", async () => {
    const binary = { text: "\\xff", raw: "ff" };
    const { m, opened, onSelect, setProps } = model(() => Promise.resolve(page({ keys: [binary, ...keys(1)] })));
    await opened();
    m.select(0);
    expect(onSelect).toHaveBeenCalledWith(binary);
    setProps("selected", binary);
    expect([m.isSelected(0), m.isSelected(1), m.isBinary(0), m.isBinary(1)]).toEqual([true, false, true, false]);
    m.select(2);
    expect(onSelect).toHaveBeenCalledTimes(1);
  });

  it("fetches the metadata of the rows on screen, a moment later", async () => {
    const { m, api, opened } = model(() => Promise.resolve(page({ keys: keys(3) })));
    await opened();
    expect(api.keyMeta).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(150);
    await settle();
    expect(api.keyMeta).toHaveBeenCalledWith(6, keys(3));
    expect([m.rowType(0), m.rowTtl(0)]).toEqual(["hash", "2m"]);
  });
});
