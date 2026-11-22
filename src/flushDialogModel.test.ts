import { createRoot } from "solid-js";
import { describe, expect, it, vi } from "vitest";
import { createFlushDialogModel } from "./flushDialogModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection } from "./test/fixtures";

function model(flush: () => Promise<void> = () => Promise.resolve()) {
  const api = fakeApi({ flush });
  const onClose = vi.fn();
  const onDone = vi.fn();
  const m = createRoot(() => createFlushDialogModel({ conn: connection({ id: 4, label: "orders", db: 2 }), onClose, onDone }));
  return { m, api, onClose, onDone };
}

describe("flush dialog", () => {
  it("sends nothing until the connection's label is typed exactly", async () => {
    const { m, api } = model();
    for (const wrong of ["", "order", "Orders", "orders "]) {
      m.setTyped(wrong);
      expect(m.ready()).toBe(false);
      m.run();
    }
    await settle();
    expect(api.flush).not.toHaveBeenCalled();
  });

  it("flushes the open database and hands the typed text to the backend", async () => {
    const { m, api, onClose, onDone } = model();
    m.setTyped("orders");
    expect(m.ready()).toBe(true);
    expect([m.title(), m.command(), m.scope()]).toEqual(["Flush database 2", "FLUSHDB", "in database 2"]);
    m.run();
    await settle();
    expect(api.flush).toHaveBeenCalledWith(4, false, "orders");
    expect(onDone).toHaveBeenCalledTimes(1);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("flushes every database when asked to", async () => {
    const { m, api } = model();
    m.setAll(true);
    m.setTyped("orders");
    expect([m.title(), m.command(), m.runLabel()]).toEqual(["Flush every database", "FLUSHALL", "Flush every database"]);
    m.run();
    await settle();
    expect(api.flush).toHaveBeenCalledWith(4, true, "orders");
  });

  it("cannot be started twice while it runs", async () => {
    let finish = () => {};
    const { m, api } = model(() => new Promise<void>((resolve) => (finish = resolve)));
    m.setTyped("orders");
    m.run();
    expect(m.ready()).toBe(false);
    expect(m.runLabel()).toBe("Flushing…");
    m.run();
    finish();
    await settle();
    expect(api.flush).toHaveBeenCalledTimes(1);
  });

  it("stays open and shows the error when the backend refuses", async () => {
    const { m, onClose, onDone } = model(() => Promise.reject("the name does not match"));
    m.setTyped("orders");
    m.run();
    await settle();
    expect(m.error()).toBe("the name does not match");
    expect(m.ready()).toBe(true);
    expect(onDone).not.toHaveBeenCalled();
    expect(onClose).not.toHaveBeenCalled();
  });
});
