// Builders for the backend's payloads: a plausible default, with the
// fields a test cares about passed in.

import type { Connection, ConsoleReply, Databases, DeleteProgress, IndexInfo, LiveStatus, ScanProgress, StoredProfile } from "../api";

export const indexInfo = (over: Partial<IndexInfo> = {}): IndexInfo => ({
  keys: 0,
  status: "idle",
  generation: 1,
  scannedAt: null,
  fullAt: null,
  subtrees: 0,
  ephemeral: false,
  path: "/tmp/index/db0.sqlite",
  ...over,
});

export const liveStatus = (over: Partial<LiveStatus> = {}): LiveStatus => ({
  session: 1,
  phase: "off",
  nodesLive: 0,
  nodes: 1,
  events: 0,
  appliedAdded: 0,
  appliedRemoved: 0,
  error: null,
  ...over,
});

export const connection = (over: Partial<Connection> = {}): Connection => ({
  id: 1,
  label: "local",
  tag: "dev",
  server: "valkey",
  version: "9.0.0",
  mode: "standalone",
  role: "primary",
  protocol: 3,
  db: 0,
  dbsize: 1000,
  usedMemory: 1 << 20,
  denied: [],
  index: indexInfo(),
  indexMode: "full",
  indexModeAuto: false,
  scanning: false,
  scanQueue: 0,
  indexStale: false,
  nodes: [{ addr: "127.0.0.1:6379", role: "primary", online: true }],
  cluster: null,
  note: null,
  scanProfile: "fast",
  scanTarget: "auto",
  scanNotes: [],
  live: liveStatus(),
  notify: null,
  ...over,
});

export const scanProgress = (over: Partial<ScanProgress> = {}): ScanProgress => ({
  session: 1,
  job: "full",
  subtree: null,
  fraction: null,
  estimate: false,
  scanQueue: 0,
  phase: "scanning",
  generation: 1,
  resumed: false,
  partial: false,
  dbsize: 1000,
  scanned: 0,
  indexedNew: 0,
  calls: 0,
  count: 100,
  lastRttMs: 0.2,
  elapsedMs: 0,
  queued: 0,
  pruned: 0,
  indexKeys: 0,
  error: null,
  nodes: [],
  backoff: 0,
  scanNotes: [],
  ...over,
});

export const storedProfile = (over: Partial<StoredProfile> = {}): StoredProfile => ({
  id: "11111111-1111-4111-8111-111111111111",
  name: "Cache",
  host: "cache.internal",
  port: 6379,
  db: 0,
  username: null,
  tls: false,
  tlsInsecure: false,
  tag: "dev",
  scanProfile: null,
  scanTarget: "auto",
  ephemeralIndex: null,
  indexMode: null,
  ssh: null,
  passwordSaved: false,
  sshSecretSaved: false,
  indexBytes: 0,
  ...over,
});

export const databases = (over: Partial<Databases> = {}): Databases => ({
  current: 0,
  count: 16,
  configured: true,
  used: [],
  ...over,
});

export const deleteProgress = (over: Partial<DeleteProgress> = {}): DeleteProgress => ({
  session: 1,
  query: "user:*",
  phase: "deleting",
  total: 10,
  matched: 10,
  deleted: 0,
  removed: 0,
  calls: 0,
  elapsedMs: 0,
  error: null,
  ...over,
});

export const consoleReply = (over: Partial<ConsoleReply> = {}): ConsoleReply => ({
  status: "ok",
  lines: ["OK"],
  truncated: false,
  node: null,
  tookMs: 1.2,
  confirm: null,
  indexNote: null,
  ...over,
});
