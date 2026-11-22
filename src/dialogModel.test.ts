import { createRoot } from "solid-js";
import { describe, expect, it } from "vitest";
import { ask, confirmWrite, createDialogHostModel } from "./dialogModel";
import { connection } from "./test/fixtures";

const host = () => createRoot(() => createDialogHostModel());

describe("ask", () => {
  it("resolves with the user's answer and clears the question", async () => {
    const h = host();
    const yes = ask({ title: "Delete?", confirm: "Delete" });
    expect(h.asked()?.title).toBe("Delete?");
    h.confirm();
    expect(await yes).toBe(true);
    expect(h.asked()).toBeNull();

    const no = ask({ title: "Delete?", confirm: "Delete" });
    h.cancel();
    expect(await no).toBe(false);
  });

  it("answers a question with no when another one replaces it", async () => {
    const h = host();
    const first = ask({ title: "First", confirm: "OK" });
    const second = ask({ title: "Second", confirm: "OK" });
    expect(await first).toBe(false);
    expect(h.asked()?.title).toBe("Second");
    h.confirm();
    expect(await second).toBe(true);
  });

  it("is not ready when nothing is asked", () => {
    const h = host();
    expect(h.ready()).toBe(false);
    expect(h.confirmIfReady()).toBe(false);
  });
});

describe("typed confirmation", () => {
  it("enables the button only for the exact text", async () => {
    const h = host();
    const answer = ask({ title: "Flush?", confirm: "Flush", typeToConfirm: "prod-cache" });
    expect(h.ready()).toBe(false);

    for (const wrong of ["prod-cach", "prod-cache ", "PROD-CACHE"]) {
      h.setTyped(wrong);
      expect(h.ready()).toBe(false);
    }
    h.confirmIfReady();
    expect(h.asked()).not.toBeNull();

    h.setTyped("prod-cache");
    expect(h.ready()).toBe(true);
    h.confirmIfReady();
    expect(await answer).toBe(true);
  });

  it("forgets the typed text for the next question", async () => {
    const h = host();
    void ask({ title: "One", confirm: "OK", typeToConfirm: "yes" });
    h.setTyped("yes");
    h.confirm();
    void ask({ title: "Two", confirm: "OK", typeToConfirm: "yes" });
    expect(h.typed()).toBe("");
    expect(h.ready()).toBe(false);
    h.cancel();
  });
});

describe("confirmWrite", () => {
  it("does not ask on dev and staging", async () => {
    const h = host();
    expect(await confirmWrite(connection({ tag: "dev" }), "DEL a")).toBe(true);
    expect(await confirmWrite(connection({ tag: "staging" }), "DEL a")).toBe(true);
    expect(h.asked()).toBeNull();
  });

  it("asks on prod, naming the connection and the command", async () => {
    const h = host();
    const answer = confirmWrite(connection({ tag: "prod", label: "orders" }), "DEL a");
    expect(h.asked()).toMatchObject({ title: "orders is tagged prod", body: "DEL a", danger: true });
    h.cancel();
    expect(await answer).toBe(false);
  });
});
