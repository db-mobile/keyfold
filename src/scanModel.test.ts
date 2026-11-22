import { createRoot } from "solid-js";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Connection, ScanProgress } from "./api";
import { createDialogHostModel } from "./dialogModel";
import { ago, createScanModel } from "./scanModel";
import { fakeApi, settle } from "./test/fakeApi";
import { connection, indexInfo, liveStatus, scanProgress } from "./test/fixtures";

const NOW = 1_800_000_000;

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW * 1000);
});
afterEach(() => vi.useRealTimers());

describe("ago", () => {
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

/// A model over a fixed connection and progress.
function model(conn: Partial<Connection> = {}, progress: Partial<ScanProgress> | null = null) {
  const onRefresh = vi.fn();
  const onMode = vi.fn(() => Promise.resolve());
  const m = createRoot(() =>
    createScanModel(
      () => connection(conn),
      () => (progress ? scanProgress(progress) : null),
      onRefresh,
      onMode,
    ),
  );
  return { m, onRefresh, onMode };
}

describe("status", () => {
  const status = (conn: Partial<Connection> = {}, progress: Partial<ScanProgress> | null = null) => model(conn, progress).m.status();

  it("says so when there is no index, whatever the scan does", () => {
    expect(status({ indexMode: "none" }, { phase: "scanning" })).toBe("No index: keys are read page by page");
  });

  it("counts keys while a first scan has no fraction yet", () => {
    expect(status({}, { phase: "scanning", scanned: 12345 })).toBe("Scanning, 12,345 keys");
  });

  it("shows the share of a first scan", () => {
    expect(status({}, { phase: "scanning", fraction: 0.5 })).toBe("Scanning 50.0%");
  });

  it("calls an index stale while a rescan refreshes it", () => {
    const conn = { index: indexInfo({ fullAt: NOW - 120 }) };
    expect(status(conn, { phase: "scanning", fraction: 0.25 })).toBe("Stale (scanned 2 min ago), refreshing 25.0%");
  });

  it("treats a connection that reports a scan without progress as starting", () => {
    const { m } = model({ scanning: true });
    expect(m.phase()).toBe("starting");
    expect(m.active()).toBe(true);
    expect(m.status()).toBe("Scanning, 0 keys");
  });

  it("names a sample and a subtree scan", () => {
    expect(status({}, { phase: "scanning", job: "sample" })).toBe("Sampling keys");
    const subtree = { text: "user:", raw: null };
    expect(status({}, { phase: "scanning", job: "subtree", subtree, scanned: 40 })).toBe("Scanning user:*, 40 keys");
    expect(status({}, { phase: "scanning", job: "subtree", subtree, fraction: 0.314, scanQueue: 2 })).toBe("Scanning user:*, about 31 % walked, 2 queued");
  });

  it("covers the phases after scanning", () => {
    expect(status({}, { phase: "paused", scanQueue: 1 })).toBe("Scan paused, 1 queued");
    expect(status({}, { phase: "finishing" })).toBe("Finishing scan");
    expect(status({}, { phase: "failed", error: "connection reset" })).toBe("Scan failed: connection reset");
    expect(status({}, { phase: "stopped" })).toBe("Scan stopped, resumable");
  });

  it("reports what a completed scan took", () => {
    const done = { phase: "complete" as const, elapsedMs: 1540, calls: 30, scanned: 3000 };
    expect(status({}, done)).toBe("Scanned just now (1.5 s, 30 SCAN calls)");
    expect(status({}, { ...done, job: "sample" })).toBe("Sampled 3,000 keys (1.5 s, 30 SCAN calls)");
    expect(status({}, { ...done, job: "subtree", subtree: { text: "user:", raw: null } })).toBe("Scanned user:* just now (1.5 s, 30 SCAN calls)");
  });

  it("leads with Live while live updates run", () => {
    const live = { live: liveStatus({ phase: "live" }) };
    expect(status(live, { phase: "complete", elapsedMs: 1000, calls: 3 })).toBe("Live, scanned just now (1.0 s, 3 SCAN calls)");
    expect(status({ ...live, index: indexInfo({ fullAt: NOW - 7200 }) })).toBe("Live, scanned 2 h ago");
  });

  it("falls back to what the index says when no scan ran in this session", () => {
    expect(status({ index: indexInfo({ fullAt: NOW - 30 }) })).toBe("Scanned 30 s ago");
    expect(status({ index: indexInfo({ status: "running" }) })).toBe("Interrupted scan, resumable");
    expect(status({ indexMode: "onDemand", index: indexInfo({ subtrees: 1 }) })).toBe("On demand: 1 subtree scanned");
    expect(status({ indexMode: "onDemand", index: indexInfo({ subtrees: 3 }) })).toBe("On demand: 3 subtrees scanned");
    expect(status()).toBe("Not indexed");
  });
});

describe("footer actions", () => {
  it("labels the rescan by what it would do", () => {
    expect(model().m.rescanLabel()).toBe("Rescan");
    expect(model({ indexMode: "onDemand" }).m.rescanLabel()).toBe("Scan everything");
    const resumable = model({ indexMode: "onDemand", index: indexInfo({ status: "running" }) }).m;
    expect(resumable.rescanLabel()).toBe("Resume scan");
    expect(resumable.rescanIcon()).toBe("play");
  });

  it("offers pause only while keys are read, and rescan only when idle", () => {
    const scanning = model({}, { phase: "scanning" }).m;
    expect([scanning.canPause(), scanning.isPaused(), scanning.canRescan()]).toEqual([true, false, false]);
    const paused = model({}, { phase: "paused" }).m;
    expect([paused.canPause(), paused.isPaused(), paused.canRescan()]).toEqual([false, true, false]);
    const idle = model().m;
    expect([idle.canPause(), idle.isPaused(), idle.canRescan()]).toEqual([false, false, true]);
    expect(model({ indexMode: "none" }).m.canRescan()).toBe(false);
  });

  it("lets a subtree scan or a queue be stopped, not a lone full scan", () => {
    expect(model({}, { phase: "scanning" }).m.canStop()).toBe(false);
    expect(model({}, { phase: "scanning", job: "subtree" }).m.canStop()).toBe(true);
    expect(model({}, { phase: "scanning", scanQueue: 1 }).m.canStop()).toBe(true);
    expect(model({ scanQueue: 1 }).m.canStop()).toBe(false);
  });

  it("describes the counters and the nodes of a running scan", () => {
    const { m } = model(
      {},
      {
        phase: "scanning",
        scanned: 500,
        dbsize: 1000,
        count: 200,
        lastRttMs: 0.34,
        indexKeys: 480,
        backoff: 2,
        nodes: [
          { label: "a:1", role: "replica", dbsize: 1000, scanned: 500, calls: 5, count: 200, done: false, backoff: 2, error: null },
          { label: "b:1", role: "replica", dbsize: 0, scanned: 0, calls: 1, count: 200, done: true, backoff: 0, error: "timeout" },
        ],
      },
    );
    expect(m.stats()).toBe("500 of 1,000, COUNT 200, rtt 0.3 ms, 480 in index, backing off (level 2)");
    expect(m.nodesLine()).toBe("Reading 2 replicas: a:1 (replica 50%, backoff 2), b:1 (replica, done)");
    expect(m.nodeErrors()).toEqual(["b:1: timeout"]);
  });

  it("shows no counters once the scan is over", () => {
    const { m } = model({}, { phase: "complete", scanned: 500 });
    expect(m.stats()).toBe("");
    expect(m.nodesLine()).toBe("");
  });
});

describe("rescan", () => {
  it("starts a full index's scan without asking", async () => {
    const api = fakeApi({ scanStart: () => Promise.resolve() });
    const { m, onRefresh } = model({ id: 7 });
    await m.rescan();
    expect(api.scanStart).toHaveBeenCalledWith(7, true);
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });

  it("asks before it indexes everything of an on-demand connection", async () => {
    const api = fakeApi({ scanStart: () => Promise.resolve() });
    const host = createRoot(() => createDialogHostModel());
    const { m } = model({ indexMode: "onDemand", dbsize: 25_000_000, tag: "prod" });

    const declined = m.rescan();
    expect(host.asked()).toMatchObject({ title: "Scan everything?", confirm: "Scan everything", danger: true });
    expect(String(host.asked()?.body)).toContain("25,000,000 keys");
    host.cancel();
    await declined;
    expect(api.scanStart).not.toHaveBeenCalled();

    const accepted = m.rescan();
    host.confirm();
    await accepted;
    expect(api.scanStart).toHaveBeenCalledTimes(1);
  });

  it("keeps the reason a scan could not start, and offers the primary for it", async () => {
    fakeApi({ scanStart: () => Promise.reject("no replica; choose the primary target") });
    const { m } = model();
    await m.rescan();
    expect(m.scanError()).toBe("no replica; choose the primary target");
    expect(m.offersPrimary()).toBe(true);
  });

  it("sets the options for the next scan and refreshes the connection", async () => {
    const api = fakeApi({ scanOptions: () => Promise.resolve() });
    const { m, onRefresh } = model({ id: 3, scanProfile: "fast", scanTarget: "auto" });
    m.setProfile("polite");
    m.setTarget("replica");
    await settle();
    expect(api.scanOptions.mock.calls).toEqual([
      [3, "polite", "auto"],
      [3, "fast", "replica"],
    ]);
    expect(onRefresh).toHaveBeenCalledTimes(2);
  });

  it("passes an index mode on and shows why a change failed", async () => {
    const { m, onMode } = model();
    m.setMode("onDemand");
    m.setMode("");
    expect(m.modeBusy()).toBe(true);
    await settle();
    expect(onMode.mock.calls).toEqual([["onDemand"], [null]]);
    expect(m.modeBusy()).toBe(false);

    onMode.mockRejectedValueOnce("disconnect first");
    m.setMode("none");
    await settle();
    expect(m.scanError()).toBe("disconnect first");
  });
});
