import { createRoot } from "solid-js";
import { describe, expect, it, vi } from "vitest";
import type { Connection, DeletePreview } from "./api";
import { createDeleteDialogModel } from "./deleteDialogModel";
import { createDialogHostModel } from "./dialogModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection, deleteProgress, indexInfo } from "./test/fixtures";

const preview = (over: Partial<DeletePreview> = {}): DeletePreview => ({ mode: "glob", count: 1200, sample: ["user:1", "user:2"], tookMs: 3.4, ...over });
const target = { query: "user:*", prefix: null };

async function model(conn: Partial<Connection> = {}, impl: Parameters<typeof fakeApi>[0] = {}) {
  const api = fakeApi({ deletePreview: () => Promise.resolve(preview()), deleteStart: () => Promise.resolve(), ...impl });
  const onClose = vi.fn();
  let dispose = () => {};
  const m = createRoot((d) => {
    dispose = d;
    return createDeleteDialogModel({ conn: connection({ id: 5, ...conn }), target, onClose });
  });
  await settle();
  return { m, api, onClose, dispose };
}

describe("delete dialog", () => {
  it("previews the matches from the index before anything is deleted", async () => {
    const { m, api } = await model();
    expect(api.deletePreview).toHaveBeenCalledWith(5, target);
    expect(m.idle()).toBe(true);
    expect(m.canDelete()).toBe(true);
    expect(m.deleteCount()).toBe("1,200");
    const p = m.preview()!;
    expect([m.noun(p), m.hasMore(p), m.more(p)]).toEqual(["keys", true, "1,198"]);
    expect(api.deleteStart).not.toHaveBeenCalled();
  });

  it("offers nothing to delete when nothing matches", async () => {
    const { m } = await model({}, { deletePreview: () => Promise.resolve(preview({ count: 0, sample: [] })) });
    expect(m.canDelete()).toBe(false);
  });

  it("shows why the preview failed", async () => {
    const { m } = await model({}, { deletePreview: () => Promise.reject("bad pattern") });
    expect(m.error()).toBe("bad pattern");
    expect(m.counting()).toBe(false);
    expect(m.canDelete()).toBe(false);
  });

  it("starts without asking on dev and cannot be dismissed while it runs", async () => {
    const { m, api, onClose } = await model();
    await m.start();
    expect(api.deleteStart).toHaveBeenCalledWith(5, target, true);
    expect(m.running()).toBe(true);
    m.close();
    expect(onClose).not.toHaveBeenCalled();
  });

  it("asks first on prod and does nothing when the answer is no", async () => {
    const host = createRoot(() => createDialogHostModel());
    const { m, api } = await model({ tag: "prod", label: "orders" });

    const declined = m.start();
    await settle();
    expect(host.asked()).toMatchObject({ title: "orders is tagged prod", body: "UNLINK 1,200 keys matching user:*" });
    host.cancel();
    await declined;
    expect(api.deleteStart).not.toHaveBeenCalled();
    expect(m.running()).toBe(false);

    const accepted = m.start();
    await settle();
    host.confirm();
    await accepted;
    expect(api.deleteStart).toHaveBeenCalledTimes(1);
  });

  it("is idle again when the delete could not start", async () => {
    const { m } = await model({}, { deleteStart: () => Promise.reject("a scan is running") });
    await m.start();
    expect(m.error()).toBe("a scan is running");
    expect(m.running()).toBe(false);
  });

  it("follows the progress events of its own delete", async () => {
    const { m, api, onClose } = await model();
    api.emit("onDeleteProgress", deleteProgress({ session: 5, phase: "deleting", deleted: 500, total: 1200 }));
    expect(m.running()).toBe(true);
    expect(m.finished()).toBe(false);
    expect(m.barWidth(m.progress()!)).toBe(`${(500 / 1200) * 100}%`);

    api.emit("onDeleteProgress", deleteProgress({ session: 5, phase: "paused" }));
    expect([m.running(), m.paused()]).toEqual([true, true]);

    api.emit("onDeleteProgress", deleteProgress({ session: 5, phase: "complete", deleted: 1200, total: 1200 }));
    expect([m.running(), m.finished(), m.idle()]).toEqual([false, true, false]);
    m.close();
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("ignores the progress of another session or another pattern", async () => {
    const { m, api } = await model();
    api.emit("onDeleteProgress", deleteProgress({ session: 6 }));
    api.emit("onDeleteProgress", deleteProgress({ session: 5, query: "order:*" }));
    expect(m.progress()).toBeNull();
  });

  it("stops listening when the dialog goes away", async () => {
    const { api, dispose } = await model();
    expect(api.listeners("onDeleteProgress")).toBe(1);
    dispose();
    expect(api.listeners("onDeleteProgress")).toBe(0);
  });

  it("warns that an on-demand index does not know every key", async () => {
    const partial = await model({ indexMode: "onDemand" });
    expect(partial.m.partialNote()).toContain("keys it has not seen are not counted and not deleted");
    const full = await model({ indexMode: "onDemand", index: indexInfo({ fullAt: 1 }) });
    expect(full.m.partialNote()).toBe("");
  });
});
