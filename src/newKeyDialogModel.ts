// State and actions of the new-key dialog: the first write of the chosen
// type creates the key on the server and write-through puts it in the
// index. The markup is in `NewKeyDialog.tsx`.

import { createSignal } from "solid-js";
import { api, type Connection, type MutationOp } from "./api";
import { confirmWrite } from "./dialogModel";
import { textKey } from "./keyRef";

type NewType = "string" | "hash" | "set" | "zset" | "list" | "stream";

export interface NewKeyDialogProps {
  conn: Connection;
  onClose: () => void;
  onCreated: (key: string) => void;
}

export function createNewKeyDialogModel(props: NewKeyDialogProps) {
  const [name, setName] = createSignal("");
  const [type, setType] = createSignal<NewType>("string");
  const [a, setA] = createSignal("");
  const [b, setB] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const op = (): MutationOp | null => {
    switch (type()) {
      case "string":
        return { op: "setString", value: a(), keepTtl: false };
      case "hash":
        return a() ? { op: "hashSet", field: a(), value: b() } : null;
      case "set":
        return a() ? { op: "setAdd", member: a() } : null;
      case "zset":
        return a() && !Number.isNaN(Number(b())) ? { op: "zAdd", member: a(), score: Number(b() || "0") } : null;
      case "list":
        return { op: "listPush", value: a(), head: false };
      case "stream":
        return a() ? { op: "streamAdd", fields: [[a(), b()]] } : null;
    }
  };
  const ready = () => !!name().trim() && op() !== null && !busy();

  const create = async () => {
    const o = op();
    if (!ready() || !o) return;
    const key = name().trim();
    if (!(await confirmWrite(props.conn, `create ${key} as ${type()}`))) return;
    setBusy(true);
    setError(null);
    try {
      const r = await api.mutate(props.conn.id, textKey(key), o, true);
      api.log("info", `created ${key}: ${JSON.stringify(r)}`);
      props.onCreated(key);
      props.onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return {
    name,
    type,
    a,
    b,
    error,
    ready,
    setName,
    setType: (value: string) => setType(value as NewType),
    setA,
    setB,
    /// Which inputs the chosen type asks for.
    isString: () => type() === "string",
    isFieldValue: () => type() === "hash" || type() === "stream",
    isSingle: () => type() === "set" || type() === "list",
    isZset: () => type() === "zset",
    singleLabel: () => (type() === "set" ? "Member" : "First item"),
    createLabel: () => (busy() ? "Creating…" : "Create key"),
    create: () => void create(),
    focusLater: (el: HTMLElement) => setTimeout(() => el.focus()),
  };
}
