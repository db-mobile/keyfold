import { createRoot } from "solid-js";
import { describe, expect, it, vi } from "vitest";
import type { Connection, ConsoleReply } from "./api";
import { createConsoleModel } from "./consoleModel";
import { createDialogHostModel } from "./dialogModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection, consoleReply } from "./test/fixtures";
import { scrollBox } from "./test/scroll";

type Run = (id: number, line: string, node: string | null, confirmed: string | null) => Promise<ConsoleReply>;

/// A console over a fake backend. Rows come from the virtualizer, which
/// only hands out the ones that fit the stated box (see `test/scroll.ts`).
function model(run: Run = () => Promise.resolve(consoleReply()), conn: Partial<Connection> = {}) {
  const api = fakeApi({ consoleRun: vi.fn(run) });
  const onRefresh = vi.fn();
  const onClose = vi.fn();
  const m = createRoot(() => {
    const m = createConsoleModel({ conn: connection({ id: 2, ...conn }), onClose, onRefresh });
    m.setScrollEl(scrollBox(2400));
    m.setInputEl(document.createElement("input"));
    return m;
  });
  const key = (key: string, more: Partial<KeyboardEvent> = {}) => m.onKeyDown({ key, preventDefault: () => {}, ...more } as KeyboardEvent);
  const type = async (line: string) => {
    m.setInput(line);
    key("Enter");
    await settle(10);
  };
  const rows = () => m.items().map((item) => `${m.rowClass(item).replace("console-line ", "")}: ${m.rowText(item)}`);
  return { m, api, onRefresh, onClose, key, type, rows };
}

describe("running a line", () => {
  it("echoes the line, prints the reply and a note with its timing", async () => {
    const { m, api, type, rows } = model(() => Promise.resolve(consoleReply({ lines: ['"1"'], node: "127.0.0.1:6379", tookMs: 0.42 })));
    await type("  GET a  ");
    expect(api.consoleRun).toHaveBeenCalledWith(2, "GET a", null, null);
    expect(rows()).toEqual(["cmd: > GET a", 'out: "1"', "note: 127.0.0.1:6379, 0.4 ms"]);
    expect(m.input()).toBe("");
    expect(m.busy()).toBe(false);
  });

  it("sends nothing for an empty line", async () => {
    const { api, type } = model();
    await type("   ");
    expect(api.consoleRun).not.toHaveBeenCalled();
  });

  it("marks a refused line as not sent and gives no timing", async () => {
    const { type, rows } = model(() => Promise.resolve(consoleReply({ status: "refused", lines: ["KEYS is never sent"], tookMs: 0 })));
    await type("KEYS *");
    expect(rows()).toEqual(["cmd: > KEYS *", "err: Not sent: KEYS is never sent"]);
  });

  it("prints a server error and a failed call as errors", async () => {
    const replies: Run[] = [() => Promise.resolve(consoleReply({ status: "error", lines: ["ERR wrong type"] })), () => Promise.reject("connection lost")];
    const { type, rows } = model((...args) => replies.shift()!(...args));
    await type("INCR a");
    await type("PING");
    expect(rows()).toEqual(["cmd: > INCR a", "err: ERR wrong type", "note: 1.2 ms", "cmd: > PING", "err: connection lost"]);
  });

  it("notes a cut reply, and refreshes the connection when the index was touched", async () => {
    const { type, rows, onRefresh } = model(() => Promise.resolve(consoleReply({ truncated: true, indexNote: "index: 1 key removed" })));
    await type("DEL a");
    expect(rows().at(-1)).toBe("note: 1.2 ms, reply cut, index: 1 key removed");
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });

  it("sends a command without keys to the chosen cluster node", async () => {
    const { m, api, type, rows } = model(undefined, {
      cluster: { shards: 3, nodes: 6, unassignedSlots: 0 },
      nodes: [
        { addr: "a:1", role: "primary", online: true },
        { addr: "b:1", role: "replica", online: true },
        { addr: "c:1", role: "primary", online: false },
      ],
    });
    expect(m.isCluster()).toBe(true);
    expect(m.primaries()).toEqual(["a:1"]);
    m.setNode("a:1");
    await type("DBSIZE");
    expect(api.consoleRun).toHaveBeenCalledWith(2, "DBSIZE", "a:1", null);
    expect(rows()[0]).toBe("cmd: a:1 > DBSIZE");
  });

  it("clears the scrollback on Ctrl+L and closes on Escape", async () => {
    const { m, type, key, onClose } = model();
    await type("PING");
    expect(m.empty()).toBe(false);
    key("l", { ctrlKey: true });
    expect(m.empty()).toBe(true);
    key("Escape");
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

describe("confirmation", () => {
  const asks = (expect: string | null): Run => (_id, _line, _node, confirmed) =>
    Promise.resolve(confirmed === null ? consoleReply({ status: "confirm", lines: [], confirm: { level: expect ? "typed" : "danger", text: "This deletes every key.", expect } }) : consoleReply());

  it("runs the line again with the typed text once the user confirms", async () => {
    const host = createRoot(() => createDialogHostModel());
    const { api, type, rows } = model(asks("orders"));
    await type("FLUSHDB");
    expect(host.asked()).toMatchObject({ title: "Run this command?", typeToConfirm: "orders", danger: true });
    expect(host.ready()).toBe(false);
    host.setTyped("orders");
    host.confirmIfReady();
    await settle(10);
    expect(api.consoleRun.mock.calls).toEqual([
      [2, "FLUSHDB", null, null],
      [2, "FLUSHDB", null, "orders"],
    ]);
    expect(rows()).toEqual(["cmd: > FLUSHDB", "out: OK", "note: 1.2 ms"]);
  });

  it("confirms with yes when no text has to be typed", async () => {
    const host = createRoot(() => createDialogHostModel());
    const { api, type } = model(asks(null));
    await type("DEL a");
    host.confirm();
    await settle(10);
    expect(api.consoleRun).toHaveBeenLastCalledWith(2, "DEL a", null, "yes");
  });

  it("does not run a line the user declined", async () => {
    const host = createRoot(() => createDialogHostModel());
    const { m, api, type, rows } = model(asks(null));
    await type("DEL a");
    host.cancel();
    await settle(10);
    expect(api.consoleRun).toHaveBeenCalledTimes(1);
    expect(rows()).toEqual(["cmd: > DEL a", "note: Not run."]);
    expect(m.busy()).toBe(false);
  });
});

describe("history", () => {
  it("walks back and forth and returns to the draft", async () => {
    const { m, type, key } = model();
    await type("GET a");
    await type("GET b");
    m.setInput("GET c, unfinished");

    key("ArrowDown");
    expect(m.input()).toBe("GET c, unfinished");
    key("ArrowUp");
    expect(m.input()).toBe("GET b");
    key("ArrowUp");
    expect(m.input()).toBe("GET a");
    key("ArrowUp");
    expect(m.input()).toBe("GET a");
    key("ArrowDown");
    expect(m.input()).toBe("GET b");
    key("ArrowDown");
    expect(m.input()).toBe("GET c, unfinished");
  });

  it("keeps a line that is repeated only once", async () => {
    const { m, type, key } = model();
    await type("GET a");
    await type("PING");
    await type("PING");
    key("ArrowUp");
    key("ArrowUp");
    expect(m.input()).toBe("GET a");
  });

  it("starts from the newest line again after a run", async () => {
    const { m, type, key } = model();
    await type("GET a");
    await type("GET b");
    key("ArrowUp");
    key("ArrowUp");
    await type("GET c");
    key("ArrowUp");
    expect(m.input()).toBe("GET c");
  });

  it("does nothing without a history", () => {
    const { m, key } = model();
    m.setInput("draft");
    key("ArrowUp");
    expect(m.input()).toBe("draft");
  });
});
