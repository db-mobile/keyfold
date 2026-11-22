import { describe, expect, it } from "vitest";
import { globEscape, isBinary, keyId, sameKey, textKey } from "./keyRef";

describe("keyId", () => {
  it("keeps a text name apart from a binary name whose hex reads the same", () => {
    expect(keyId(textKey("ff00"))).toBe("t:ff00");
    expect(keyId({ text: "\\xff\\x00", raw: "ff00" })).toBe("x:ff00");
  });

  it("identifies a binary name by its bytes, not by its display text", () => {
    expect(keyId({ text: "a", raw: "ff" })).toBe(keyId({ text: "b", raw: "ff" }));
  });
});

describe("sameKey", () => {
  it("compares two references by identity", () => {
    expect(sameKey(textKey("a"), textKey("a"))).toBe(true);
    expect(sameKey(textKey("a"), textKey("b"))).toBe(false);
    expect(sameKey(textKey("ff"), { text: "ff", raw: "ff" })).toBe(false);
  });

  it("is false when either side is missing", () => {
    expect(sameKey(null, textKey("a"))).toBe(false);
    expect(sameKey(textKey("a"), undefined)).toBe(false);
    expect(sameKey(null, null)).toBe(false);
  });
});

describe("isBinary", () => {
  it("is true only for a name that carries raw bytes", () => {
    expect(isBinary(textKey("a"))).toBe(false);
    expect(isBinary({ text: "\\xff", raw: "ff" })).toBe(true);
    expect(isBinary(null)).toBe(false);
  });
});

describe("globEscape", () => {
  it("escapes every glob metacharacter and the backslash", () => {
    expect(globEscape("a*b?c[d]e\\f")).toBe("a\\*b\\?c\\[d\\]e\\\\f");
  });

  it("leaves a plain name alone", () => {
    expect(globEscape("user:1:profile")).toBe("user:1:profile");
  });
});
