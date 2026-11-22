import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ago } from "./scanModel";

const NOW = 1_800_000_000;

describe("ago", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW * 1000);
  });
  afterEach(() => vi.useRealTimers());

  it("picks the unit by age", () => {
    expect(ago(NOW)).toBe("0 s ago");
    expect(ago(NOW - 59)).toBe("59 s ago");
    expect(ago(NOW - 60)).toBe("1 min ago");
    expect(ago(NOW - 3600)).toBe("1 h ago");
    expect(ago(NOW - 86400 * 3)).toBe("3 days ago");
  });

  it("never reports a time in the future", () => {
    expect(ago(NOW + 30)).toBe("0 s ago");
  });
});
