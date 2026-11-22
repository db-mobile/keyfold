import { describe, expect, it } from "vitest";
import { loadRecent, pushRecent, recentKey } from "./jumpPaletteModel";
import { textKey } from "./keyRef";

const store = (scope: string, value: unknown) => localStorage.setItem(recentKey(scope), JSON.stringify(value));

describe("loadRecent", () => {
  it("is empty when nothing was stored", () => {
    expect(loadRecent("a")).toEqual([]);
  });

  it("keeps the scopes apart", () => {
    pushRecent("a", textKey("one"));
    expect(loadRecent("b")).toEqual([]);
  });

  it("reads plain names stored before names carried their bytes", () => {
    store("a", ["old", { text: "\\xff", raw: "ff" }]);
    expect(loadRecent("a")).toEqual([textKey("old"), { text: "\\xff", raw: "ff" }]);
  });

  it("drops entries of an unknown shape", () => {
    store("a", [1, null, { text: 1, raw: null }, { text: "x" }, { text: "ok", raw: null }]);
    expect(loadRecent("a")).toEqual([textKey("ok")]);
  });

  it("is empty for broken or foreign content", () => {
    localStorage.setItem(recentKey("a"), "{not json");
    expect(loadRecent("a")).toEqual([]);
    store("a", { text: "not a list" });
    expect(loadRecent("a")).toEqual([]);
  });
});

describe("pushRecent", () => {
  it("puts the newest key first and stores the list", () => {
    pushRecent("a", textKey("one"));
    expect(pushRecent("a", textKey("two"))).toEqual([textKey("two"), textKey("one")]);
    expect(loadRecent("a")).toEqual([textKey("two"), textKey("one")]);
  });

  it("moves a key that is opened again to the front", () => {
    pushRecent("a", textKey("one"));
    pushRecent("a", textKey("two"));
    expect(pushRecent("a", textKey("one"))).toEqual([textKey("one"), textKey("two")]);
  });

  it("treats a binary name and a text name of the same spelling as two keys", () => {
    pushRecent("a", textKey("ff"));
    expect(pushRecent("a", { text: "ff", raw: "ff" })).toHaveLength(2);
  });

  it("keeps the 20 most recent keys", () => {
    for (let i = 0; i < 25; i++) pushRecent("a", textKey(`k${i}`));
    const recent = loadRecent("a");
    expect(recent).toHaveLength(20);
    expect(recent[0]).toEqual(textKey("k24"));
    expect(recent[19]).toEqual(textKey("k5"));
  });
});
