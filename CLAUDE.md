# Project guide

A desktop client for Redis and Valkey, built to browse databases with millions of
keys. Tauri 2, Rust backend, SolidJS frontend.

## Source of truth

`CONCEPT.md` holds the complete design. All concept-level decisions are closed
and dated (section 11). Read it before any implementation work. Do not reopen
decided items; propose changes as an explicit "amend the concept" step instead.

## Stack (decided)

- Tauri 2 shell, Rust backend, SolidJS + TypeScript frontend
- Redis client: the `redis` crate (redis-rs), raw `Cmd` only, no crate cluster abstraction
- Index: SQLite via `rusqlite` (bundled), schema in CONCEPT.md section 3.1
- Servers: Redis and Valkey supported equally; never gate on `redis_version`
- CI matrix: Redis 7.2, Redis 8.x, Valkey 8.x, Valkey 9.x in containers

## Core invariants

- The webview never holds the keyspace; it requests windows of at most 200 rows
- Redis/Valkey is the truth, the index is a rebuildable cache
- Never use `KEYS`; scanning is `SCAN` with adaptive COUNT, pausable
- Every command the browser issues is a raw command so the ACL list stays greppable
- Prod-tagged connections: polite scan profile, ephemeral index, confirm writes

## Build order

Follow CONCEPT.md section 12. Step one is a Rust workspace with connection
manager, capability layer, scan engine, SQLite index and a CLI harness,
benchmarked against a generated 10M-key database. No Tauri or frontend until
step two.

## Working conventions

- Thin, verifiable slices; each slice ends with tests passing on the four-server matrix
- Design questions are answered as an assessment first; the concept is edited only when asked
- Working product name: Keyfold. It must not contain "Redis" (trademark); the project folder name is incidental
