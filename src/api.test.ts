import { describe, expect, it } from "vitest";
import { fmtBytes, fmtCount, fmtTtl } from "./api";

describe("fmtCount", () => {
  it("groups thousands", () => {
    expect(fmtCount(0)).toBe("0");
    expect(fmtCount(999)).toBe("999");
    expect(fmtCount(10_000_000)).toBe("10,000,000");
  });
});

describe("fmtBytes", () => {
  it("shows nothing for an unknown size", () => {
    expect(fmtBytes(null)).toBe("");
  });

  it("keeps bytes whole and gives larger units one decimal", () => {
    expect(fmtBytes(0)).toBe("0 B");
    expect(fmtBytes(1023)).toBe("1023 B");
    expect(fmtBytes(1024)).toBe("1.0 KB");
    expect(fmtBytes(1536)).toBe("1.5 KB");
    expect(fmtBytes(5 * 1024 ** 3)).toBe("5.0 GB");
  });

  it("stops at terabytes", () => {
    expect(fmtBytes(2048 * 1024 ** 4)).toBe("2048.0 TB");
  });
});

describe("fmtTtl", () => {
  it("shows nothing for a key without expiry", () => {
    expect(fmtTtl(null)).toBe("");
  });

  it("picks the unit by size", () => {
    expect(fmtTtl(59)).toBe("59s");
    expect(fmtTtl(60)).toBe("1m");
    expect(fmtTtl(3599)).toBe("60m");
    expect(fmtTtl(3600)).toBe("1h");
    expect(fmtTtl(86400)).toBe("1d");
    expect(fmtTtl(86400 * 30)).toBe("30d");
  });
});
