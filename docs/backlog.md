# Backlog: open items from build-order steps one to eight

Collected on 2026-10-26 after step eight, the last item of the build order
in `CONCEPT.md` section 12. Nothing here is decided; concept amendments
happen only on request (section 11 is closed). Each item names the step
that left it open and what it would take.

## Concept-level findings to record or decide

| Item | Origin | What is known |
|---|---|---|
| Index is several files per logical db (`db0.sqlite` plus `db0.nodes{i}.sqlite`), not one | step one | CONCEPT 3.4 says one SQLite file per logical db; the node shards exist because SQLite has one writer per file and the shard threads are what make ingestion keep up. Amend 3.4 or accept as implementation detail |
| Schema version 3 adds `keys.fts_rowid` | step seven | CONCEPT 3.1 lists the `keys` columns without it; the column lets a key's trigram row be deleted without a 6 ms lookup. Amend 3.1 |
| Trigram build is a scan-time or on-demand cost of 6.2 s per 1M keys (132 s at 10M) and 1.6 times the index size | step six and seven | CONCEPT 3.3 says "2 to 3 times the index size, slower scans"; the measured numbers could replace the estimate |
| Webview RSS baseline exceeds the 150 MB target | step two | WebKitGTK alone sits at about 159 MB on the empty connect form, 204 MB with a 1M-key list; CONCEPT 9 sets 150 MB. Either the target moves or the budget is defined above the webview baseline |
| Full scan of 10M keys: 63 to 68 s, peak RSS about 600 MB | step one | CONCEPT 9 targets under 60 s and under 300 MB during the scan. Known levers: build the tree once at the end of a first scan into an empty index, overlap key inserts fully with shard work, pack arena entries tighter |
| Prod default profile: normal (CONCEPT 4) or polite (CONCEPT 8, `CLAUDE.md`) | step eight | The code follows polite; polite means replicas only unless the target is set to primary |
| Node tag on index rows | step eight | CONCEPT 4 mentions a node tag; the owning node is derived from the slot and the current topology instead, because a stored tag goes stale on every reshard |
| Flush confirmation uses the connection label | step seven | CONCEPT 8 says "type the database name"; logical databases have no name, so the typed text is the connection's label |
| Credentials go to the keychain through the `keyring` crate, not a Tauri plugin | connection manager | CONCEPT 3.4 says "OS keychain via a Tauri plugin"; there is no official plugin, and the community ones wrap this crate. The backend calls it directly, so the webview never reads a password. Amend the wording or accept |
| Product name | all | Must not contain "Redis"; working name is Keyfold, not yet checked against trademarks and domains |

## Connection profiles and settings (CONCEPT 3.4)

- Stored connections exist (`src-tauri/src/profiles.rs`): `profiles.json`
  in the app data dir, a UUID per profile as index folder and keychain
  entry, index size, "clear index", delete. Still open from 3.4: a
  configurable cache root; user-only permissions on index files.
- A quick connect by URL stores nothing and keeps the `host_port_db` index
  folder; nothing in the UI lists or clears those folders.
- The delimiter of an index is detected once and kept: a database whose
  keys were replaced by ones with another delimiter shows a flat tree
  until its index is cleared (seen on 2026-11-22 with a test database).
- A stored connection whose password is missing from the keychain can only
  be repaired in the editor; there is no "ask once for this session".
- Stored on the profile today: tag, default db, TLS flags, scan profile and
  target (a header change is written back), the ephemeral-index override.
  Still living with the session or the index file: trigram table on or off
  (step six), fold rules and renamed levels (step four), the live-updates
  flag (step eight).
- No reordering, grouping or duplicating of stored connections, no "test
  connection" in the editor, no import or export.
- Global settings: fold thresholds (step four), auto-index threshold
  (currently 20M keys in `session.rs`).

## Scan engine and index

- Performance levers from step one (see the table above).
- Count and numeric sorts over a level with millions of children sort the
  whole level, about 300 ms at 3.5M (steps one and three); a secondary
  index was deferred.
- Tree refresh during a scan is pull based (the webview refetches the
  windows on screen); the backend delta push in the concept's conventions
  needs the backend to track which windows are expanded (step three).
- Caching lazily fetched metadata (`TYPE`, `TTL`, `MEMORY USAGE`) into the
  index (step two); `SCAN TYPE` typed views are supported by the engine
  but not offered in the UI.
- Re-routing a running cluster scan after a failover: the node's task
  fails and a rescan picks up the new topology (step eight).
- Sentinel discovery and per-node TLS parameters (step eight).
- Index modes (CONCEPT 7) are built since 2026-11-22. Still open: a session
  in mode "none" opens empty index files in the temp directory instead of
  none at all; the sample of an on-demand index runs once (no "sample
  more"); stale subtrees are not refreshed on reconnect, only on "Rescan
  this subtree"; the share walked by a subtree scan is an estimate from
  `COUNT`, not read from the cursor; the no-index list has no `SCAN TYPE`
  filter although the pager takes one.
- The value panel follows live changes of the open key since 2026-11-22
  (`value:changed`, at most every 2 s, never while an editor is open).
  Still open: a key with more than one page loaded only gets its facts
  refreshed, not its rows, unless its type or length changed; without live
  updates there is only the reload button.

## Tree and folding (CONCEPT 5.1, 5.6)

- Custom id-class regexes, affix stripping ("left-most literal part"),
  regex filters inside a fold, folding of search results and of the flat
  list (step four).
- `fold_stats` persistence (the table exists; results are cached per
  session with 5 % drift invalidation).
- Keys with non-UTF-8 names are opened, edited and deleted by their exact
  bytes since 2026-11-22. Still open: typing such a name or pattern (new
  key, rename target, jump palette, search box, fold filter box; all take
  text literally), and the value panel's field filter, which is text.

## Flat list and search (CONCEPT 5.2, 5.3)

- Sorting by other columns and column filters (step two).
- Verifying regex results on the server (step six; only globs and
  substrings have a server pattern).
- A per-profile trigram setting and its "offer after 500 ms" prompt are
  built; the setting is not persisted (see profiles).

## Values (CONCEPT 6)

- Module type viewers: JSON, search, time series, bloom, vector sets, for
  both ecosystems' `TYPE` strings (step five); today they fall back to the
  raw `DUMP` viewer.
- msgpack and gzip decoding of string values (detection exists).
- Editing binary values (step seven). An existing binary field or member
  is addressed by its bytes; its value is still edited as lossy text.

## Writes and safety (CONCEPT 7, 8)

- Type conversion of a key (CONCEPT 7 lists it; its semantics are not
  defined).
- Pattern delete while a scan runs (the scan owns the writer; the offer is
  hidden meanwhile).
- Undo.
- The command console exists since 2026-11-22 (request/reply only). Still
  open: `MONITOR` and pub/sub as streams, transactions and `SELECT` with
  tracked state, fan-out of a command to every node, completion and
  command help, a persisted history, cancelling a slow command.
- A reply still crosses from the server to the backend whole before it is
  cut for display: `SMEMBERS` on a set of millions costs backend memory.
- `Capabilities::mark_denied` is never called (found while building the
  console); a denied command is only known from its error.

## Live updates (CONCEPT 7)

- A per-node view of key-event throughput and the cost it adds to the
  server.
- Expiry and memory caches are cleared, not refreshed, when an event
  arrives for a key on screen.

## Shell and tooling

- Screenshots of the headless app fail on this desktop (X `get_image`);
  smokes rely on the backend log.
- Sentinel, TLS parameters, Unix sockets (`Profile::from_url` refuses them).
- SSH tunnels exist since 2026-11-22 for stored connections. Still open:
  `~/.ssh/config`, `ProxyJump` chains, certificates and keyboard-interactive
  logins; the SSH agent on Windows; a tunnel for quick connect by URL; a
  file chooser for the key file; asking for a passphrase at connect time
  instead of storing it; removing a trusted host key from the app's file.
- The CLI logs in twice per command with `--ssh` (once to check the host
  key, once for the work).
- Windows and macOS have not been built; CI builds the Linux shell only.
- Releases (`.github/workflows/release.yml`, on a `vX.Y.Z` tag) carry a
  Debian package and an AppImage for amd64 and arm64. Still open, all present
  in Resonance's release workflow: macOS and Windows packages, the Tauri
  updater with signed artifacts and `latest.json`, and the AUR, Homebrew and
  winget packages.
- `normal_profile_completes_with_load_sampling` asserts that the scan never
  backs off; it failed once on Valkey 9 (1 run in 5 on 2026-11-15) while the
  machine was busy, and passed on every rerun. The assertion depends on the
  server's `instantaneous_ops_per_sec` right after the fill.
- The connection manager and the database selector have only been driven
  through `KEYFOLD_AUTOPROFILE` and `KEYFOLD_AUTOSWITCHDB`; no test clicks
  through the editor.
