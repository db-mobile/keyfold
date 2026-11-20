// State and actions of the value panel (CONCEPT section 6). The markup is
// in `ValuePanel.tsx`. Selecting a key costs one pipeline (type, TTL,
// memory, encoding, length). Collections page through their cursor
// commands, 200 entries at a time, in a virtualised list that loads the
// next page when scrolled to its end. Strings arrive as raw bytes: a
// 64 KB preview, then "load full" in 1 MB chunks up to a cap. Everything
// is rendered through the same virtualiser, so the DOM never holds more
// than the rows on screen.
//
// Writes (CONCEPT section 7): every edit here is one `mutate` call that
// the backend writes through to the index; the panel reloads the key
// afterwards. Deleting a key always confirms; prod confirms every write.

import { createVirtualizer, type VirtualItem } from "@tanstack/solid-virtual";
import { createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";
import {
  api,
  CHUNK_BYTES,
  fmtBytes,
  fmtCount,
  fmtTtl,
  MAX_FULL_BYTES,
  PREVIEW_BYTES,
  type Connection,
  type DumpInfo,
  type Entry,
  type KeyInfo,
  type KeyRef,
  type MutationOp,
  type ValueCursor,
} from "./api";
import { ask, confirmWrite } from "./dialogModel";
import { isBinary, keyId, textKey } from "./keyRef";
import { VALUE_ROW } from "./ui/density";
import { toast } from "./ui/toastModel";

const ROW = VALUE_ROW;
/// Strings up to this size are edited in place; larger ones are read-only.
const MAX_EDIT_BYTES = 1 << 20;

export type Row = { kind: "entry"; e: Entry } | { kind: "line"; text: string } | { kind: "hex"; offset: number } | { kind: "more" };
export type Editor = { kind: "rename" } | { kind: "ttl" } | { kind: "string" } | { kind: "add" } | { kind: "row"; index: number; e: Entry };

export interface ValuePanelProps {
  conn: Connection;
  selected: KeyRef | null;
  onSelect: (key: KeyRef | null) => void;
}

export function createValuePanelModel(props: ValuePanelProps) {
  let scrollEl: HTMLDivElement | undefined;
  const [info, setInfo] = createSignal<KeyInfo | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [entries, setEntries] = createSignal<Entry[]>([]);
  const [cursor, setCursor] = createSignal<ValueCursor | null>(null);
  const [done, setDone] = createSignal(true);
  const [loading, setLoading] = createSignal(false);
  const [filter, setFilter] = createSignal("");
  const [byRank, setByRank] = createSignal(true);
  const [bytes, setBytes] = createSignal<Uint8Array>(new Uint8Array());
  const [dump, setDump] = createSignal<DumpInfo | null>(null);
  const [settled, setSettled] = createSignal(true);
  const [editor, setEditor] = createSignal<Editor | null>(null);
  const [fieldA, setFieldA] = createSignal("");
  const [fieldB, setFieldB] = createSignal("");
  const [head, setHead] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  let generation = 0;

  const reset = () => {
    setEntries([]);
    setCursor(null);
    setDone(true);
    setBytes(new Uint8Array());
    setDump(null);
    setError(null);
    setEditor(null);
  };

  const loadPage = async (first: boolean) => {
    const i = info();
    const key = props.selected;
    if (!i || !key || loading()) return;
    const gen = generation;
    setLoading(true);
    try {
      let c = first ? null : cursor();
      if (first && i.viewer === "zset" && byRank() && !filter()) c = { kind: "rank", value: 0 };
      if (first && i.viewer === "list") c = { kind: "rank", value: 0 };
      const page = await api.valuePage(props.conn.id, key, i.keyType, c, filter() || null);
      if (gen !== generation) return;
      setEntries(first ? page.entries : [...entries(), ...page.entries]);
      setDone(page.done);
      if (!page.done) {
        if (i.viewer === "list" || (i.viewer === "zset" && byRank() && !filter())) setCursor({ kind: "rank", value: Number(page.next) });
        else if (i.viewer === "stream") setCursor({ kind: "streamId", value: page.next! });
        else setCursor({ kind: "scan", value: page.cursor });
      }
    } catch (e) {
      setError(String(e));
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  const loadChunk = async (offset: number, len: number) => {
    const key = props.selected;
    if (!key) return;
    const gen = generation;
    const chunk = await api.valueChunk(props.conn.id, key, offset, len);
    if (gen !== generation) return;
    const cur = bytes();
    const joined = new Uint8Array(cur.length + chunk.length);
    joined.set(cur);
    joined.set(chunk, cur.length);
    setBytes(joined);
  };

  const loadFull = async () => {
    const i = info();
    if (!i || i.length === null) return;
    setLoading(true);
    try {
      let offset = bytes().length;
      const end = Math.min(i.length, MAX_FULL_BYTES);
      while (offset < end) {
        await loadChunk(offset, Math.min(CHUNK_BYTES, end - offset));
        offset = bytes().length;
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  };

  /// One inspect, then the first page or preview.
  const load = async (key: KeyRef | null) => {
    generation++;
    const gen = generation;
    reset();
    setInfo(null);
    if (!key) return;
    try {
      const i = await api.valueInspect(props.conn.id, key);
      if (gen !== generation) return;
      setInfo(i);
      if (!i.exists) return;
      if (i.viewer === "string") await loadChunk(0, PREVIEW_BYTES);
      else if (i.viewer === "raw") setDump(await api.valueDump(props.conn.id, key));
      else await loadPage(true);
    } catch (e) {
      if (gen === generation) setError(String(e));
    }
  };

  createEffect(
    on(
      // the key's identity, so that the same key selected again does not reload
      () => [props.selected ? keyId(props.selected) : null, props.conn.id] as const,
      () => {
        setFilter("");
        void load(props.selected);
      },
    ),
  );

  // Filter or order changed: start over.
  createEffect(
    on([filter, byRank], (_v, prev) => {
      if (prev === undefined || !info() || info()!.viewer === "string" || info()!.viewer === "raw") return;
      generation++;
      setEntries([]);
      setCursor(null);
      void loadPage(true);
    }),
  );

  /// One write: confirm (always for a key delete, on prod for everything),
  /// run it, remember the server's reply, reload the key.
  const mutate = async (op: MutationOp, what: string, always = false): Promise<boolean> => {
    const key = props.selected;
    if (!key) return false;
    if (always) {
      if (!(await ask({ title: what, body: key.text, confirm: "Delete", danger: true }))) return false;
    } else if (!(await confirmWrite(props.conn, `${what} on ${key.text}`))) return false;
    setBusy(true);
    setError(null);
    try {
      const r = await api.mutate(props.conn.id, key, op, true);
      toast(`${r.command}: ${r.reply}, ${r.tookMs.toFixed(1)} ms${r.queued ? ". Index update queued until the scan ends" : ""}`, "success");
      api.log("info", `mutate ${r.command} ${key.text}: ${JSON.stringify(r)}`);
      return true;
    } catch (e) {
      setError(String(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const deleteKey = async () => {
    if (await mutate({ op: "delete" }, "Delete key", true)) props.onSelect(null);
  };

  const openEditor = (e: Editor) => {
    const i = info();
    setFieldA("");
    setFieldB("");
    // an escaped name is not something to type: a binary key starts empty
    if (e.kind === "rename") setFieldA(props.selected && !isBinary(props.selected) ? props.selected.text : "");
    if (e.kind === "ttl") setFieldA(i?.ttl !== null && i?.ttl !== undefined ? String(i.ttl) : "");
    if (e.kind === "string") setFieldA(text() ?? "");
    if (e.kind === "row") {
      setFieldA(e.e.field);
      setFieldB(i?.viewer === "zset" ? String(e.e.score ?? "") : e.e.value);
    }
    setEditor(e);
  };

  const save = async () => {
    const ed = editor();
    const i = info();
    if (!ed || !i) return;
    let ok = false;
    switch (ed.kind) {
      case "rename": {
        const to = fieldA().trim();
        const from = props.selected;
        if (!to || (from && !isBinary(from) && to === from.text)) return;
        ok = await mutate({ op: "rename", to }, `RENAME to ${to}`);
        if (ok) props.onSelect(textKey(to));
        return;
      }
      case "ttl": {
        const s = Number(fieldA());
        if (!Number.isInteger(s) || s <= 0) return;
        ok = await mutate({ op: "expire", seconds: s }, `EXPIRE ${s}`);
        break;
      }
      case "string":
        ok = await mutate({ op: "setString", value: fieldA(), keepTtl: i.ttl !== null }, "SET");
        break;
      case "add":
        switch (i.viewer) {
          case "hash":
            ok = await mutate({ op: "hashSet", field: fieldA(), value: fieldB() }, `HSET ${fieldA()}`);
            break;
          case "set":
            ok = await mutate({ op: "setAdd", member: fieldA() }, `SADD ${fieldA()}`);
            break;
          case "zset":
            ok = await mutate({ op: "zAdd", member: fieldA(), score: Number(fieldB() || "0") }, `ZADD ${fieldA()}`);
            break;
          case "list":
            ok = await mutate({ op: "listPush", value: fieldA(), head: head() }, head() ? "LPUSH" : "RPUSH");
            break;
          case "stream":
            ok = await mutate({ op: "streamAdd", fields: [[fieldA(), fieldB()]] }, "XADD");
            break;
        }
        break;
      case "row":
        switch (i.viewer) {
          case "hash":
            ok = await mutate({ op: "hashSet", field: ed.e.field, fieldRaw: ed.e.fieldRaw, value: fieldB() }, `HSET ${ed.e.field}`);
            break;
          case "zset":
            ok = await mutate({ op: "zAdd", member: ed.e.field, memberRaw: ed.e.fieldRaw, score: Number(fieldB()) }, `ZADD ${ed.e.field}`);
            break;
          case "list":
            ok = await mutate({ op: "listSet", index: ed.index, value: fieldB() }, `LSET ${ed.index}`);
            break;
        }
        break;
    }
    if (ok) {
      setEditor(null);
      void load(props.selected);
    }
  };

  const removeEntry = async (index: number, e: Entry) => {
    const i = info();
    if (!i) return;
    let op: MutationOp | null = null;
    switch (i.viewer) {
      case "hash":
        op = { op: "hashDel", field: e.field, fieldRaw: e.fieldRaw };
        break;
      case "set":
        op = { op: "setRem", member: e.field, memberRaw: e.fieldRaw };
        break;
      case "zset":
        op = { op: "zRem", member: e.field, memberRaw: e.fieldRaw };
        break;
      case "list":
        op = { op: "listRem", index };
        break;
      case "stream":
        op = { op: "streamDel", id: e.field };
        break;
    }
    if (!op) return;
    if (await mutate(op, `${op.op === "listRem" ? "LREM item " + index : "remove " + e.field}`)) void load(props.selected);
  };

  const persist = async () => {
    if (await mutate({ op: "persist" }, "PERSIST")) {
      setEditor(null);
      void load(props.selected);
    }
  };

  const text = createMemo(() => {
    const i = info();
    if (!i || i.viewer !== "string") return null;
    if (i.kind !== "json" && i.kind !== "text") return null;
    const decoded = new TextDecoder("utf-8", { fatal: false }).decode(bytes());
    if (i.kind === "json" && bytes().length >= (i.length ?? 0) && bytes().length < 1 << 20) {
      try {
        return JSON.stringify(JSON.parse(decoded), null, 2);
      } catch {
        return decoded;
      }
    }
    return decoded;
  });

  const rows = createMemo<Row[]>(() => {
    const i = info();
    if (!i || !i.exists) return [];
    if (i.viewer === "string") {
      const t = text();
      if (t !== null) return t.split("\n").map((line) => ({ kind: "line", text: line }));
      const n = Math.ceil(bytes().length / 16);
      return Array.from({ length: n }, (_, k) => ({ kind: "hex", offset: k * 16 }));
    }
    if (i.viewer === "raw") {
      const d = dump();
      if (!d) return [];
      return [{ kind: "line", text: `DUMP: ${fmtBytes(d.length)} serialized` }, { kind: "line", text: `peek: ${d.peekHex}` }];
    }
    const out: Row[] = entries().map((e) => ({ kind: "entry", e }));
    if (!done()) out.push({ kind: "more" });
    return out;
  });

  const virtualizer = createVirtualizer({
    get count() {
      return rows().length;
    },
    getScrollElement: () => scrollEl ?? null,
    estimateSize: () => ROW,
    overscan: 10,
  });

  // The "more" row scrolled into view loads the next page.
  createEffect(() => {
    if (!settled() || loading()) return;
    const r = rows();
    for (const item of virtualizer.getVirtualItems()) {
      if (r[item.index]?.kind === "more") {
        void loadPage(false);
        break;
      }
    }
  });

  createEffect(() => {
    const el = scrollEl;
    if (!el) return;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const onScroll = () => {
      setSettled(false);
      clearTimeout(timer);
      timer = setTimeout(() => setSettled(true), 80);
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    onCleanup(() => {
      el.removeEventListener("scroll", onScroll);
      clearTimeout(timer);
    });
  });

  const hexRow = (offset: number) => {
    const b = bytes().subarray(offset, offset + 16);
    const hex = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join(" ");
    const ascii = Array.from(b, (x) => (x >= 32 && x < 127 ? String.fromCharCode(x) : ".")).join("");
    return { hex, ascii };
  };

  const unit = (i: KeyInfo) =>
    ({ string: "bytes", hash: "fields", set: "members", zset: "members", list: "items", stream: "entries", raw: "" })[i.viewer];
  const isCollection = () => !!info() && ["hash", "set", "zset", "list", "stream"].includes(info()!.viewer);
  const editableString = () => {
    const i = info();
    return !!i && i.viewer === "string" && (i.kind === "json" || i.kind === "text" || i.kind === "empty") && (i.length ?? 0) <= MAX_EDIT_BYTES;
  };
  const rowEditable = () => !!info() && ["hash", "zset", "list"].includes(info()!.viewer);
  const onEditorKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") setEditor(null);
    if (e.key === "Enter" && !(e.target instanceof HTMLTextAreaElement)) void save();
  };

  const addWhat = () => (info()!.viewer === "hash" ? "field" : info()!.viewer === "stream" ? "entry" : info()!.viewer === "list" ? "item" : "member");

  const entryOf = (r: Row) => (r as { e: Entry }).e;
  const offsetOf = (r: Row) => (r as { offset: number }).offset;
  const fullEnd = () => Math.min(info()!.length ?? 0, MAX_FULL_BYTES);

  return {
    info,
    error,
    busy,
    loading,
    filter,
    byRank,
    fieldA,
    fieldB,
    head,
    editor,
    setFilter,
    setByRank,
    setFieldA,
    setFieldB,
    setHead,
    setScrollEl: (el: HTMLDivElement) => (scrollEl = el),
    focusLater: (el: HTMLElement) => setTimeout(() => el.focus()),

    // header
    keyText: () => props.selected?.text ?? "",
    keyBinary: () => isBinary(props.selected),
    renameHint: () => (isBinary(props.selected) ? "A text name; the current name has bytes that cannot be typed" : ""),
    exists: () => info()?.exists,
    canEditString: () => editableString() && bytes().length >= (info()!.length ?? 0),
    isCollection,
    addWhat,
    rename: () => openEditor({ kind: "rename" }),
    editExpiry: () => openEditor({ kind: "ttl" }),
    editString: () => openEditor({ kind: "string" }),
    add: () => openEditor({ kind: "add" }),
    deleteKey,

    // facts
    expiry: (i: KeyInfo) => (i.ttl === null ? "No expiry" : `Expires in ${fmtTtl(i.ttl)}`),
    hasMem: (i: KeyInfo) => i.mem !== null,
    mem: (i: KeyInfo) => fmtBytes(i.mem),
    showing: (i: KeyInfo) =>
      i.viewer === "string"
        ? `Showing ${fmtBytes(bytes().length)} of ${fmtBytes(i.length ?? 0)}${i.kind ? `, ${i.kind}` : ""}`
        : i.viewer === "raw"
          ? "No viewer for this type"
          : `Showing ${fmtCount(entries().length)} of ${fmtCount(i.length ?? 0)} ${unit(i)}`,

    // editor
    editorIs: (ed: Editor, kind: Editor["kind"]) => ed.kind === kind,
    onEditorKey,
    addLabel: () => (info()!.viewer === "hash" || info()!.viewer === "stream" ? "Field" : info()!.viewer === "list" ? "Item" : "Member"),
    addHasValue: () => ["hash", "stream", "zset"].includes(info()!.viewer),
    addValueLabel: () => (info()!.viewer === "zset" ? "Score" : "Value"),
    isList: () => info()!.viewer === "list",
    rowLabel: (ed: Editor) => (info()!.viewer === "list" ? `Item ${(ed as { index: number }).index}` : fieldA()),
    canPersist: (ed: Editor) => ed.kind === "ttl" && info()?.ttl !== null,
    persist,
    cancel: () => setEditor(null),
    save,
    saveLabel: () => (busy() ? "Saving…" : "Save"),

    // tools
    filterable: () => info()?.exists && ["hash", "set", "zset"].includes(info()!.viewer),
    isZset: () => info()!.viewer === "zset",
    canLoadFull: () => info()?.viewer === "string" && !info()!.previewComplete && bytes().length < fullEnd(),
    loadFull,
    loadFullLabel: () => (loading() ? "Loading…" : `Load full value (${fmtBytes(fullEnd())})`),
    capped: () => (info()!.length ?? 0) > MAX_FULL_BYTES,
    cap: () => fmtBytes(MAX_FULL_BYTES),

    // rows
    innerStyle: () => ({ height: `${virtualizer.getTotalSize()}px` }),
    virtualItems: () => virtualizer.getVirtualItems(),
    rowAt: (item: VirtualItem) => rows()[item.index],
    rowStyle: (item: VirtualItem) => ({ transform: `translateY(${item.start}px)` }),
    rowIs: (r: Row, kind: Row["kind"]) => r.kind === kind,
    entry: entryOf,
    score: (r: Row) => (entryOf(r).score !== null ? `${entryOf(r).score}  ` : ""),
    rowEditable,
    editRow: (item: VirtualItem, r: Row) => openEditor({ kind: "row", index: item.index, e: entryOf(r) }),
    removeRow: (item: VirtualItem, r: Row) => removeEntry(item.index, entryOf(r)),
    lineText: (r: Row) => (r as { text: string }).text,
    hexOffset: (r: Row) => offsetOf(r).toString(16).padStart(6, "0"),
    hex: (r: Row) => hexRow(offsetOf(r)).hex,
    ascii: (r: Row) => hexRow(offsetOf(r)).ascii,
    moreLabel: () => (loading() ? "Loading…" : "…"),
  };
}
