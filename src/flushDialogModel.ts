// State and actions of the flush dialog (CONCEPT 8): the typed connection
// name gates the button and the backend checks the name again. The markup
// is in `FlushDialog.tsx`.

import { createSignal } from "solid-js";
import { api, type Connection } from "./api";

export interface FlushDialogProps {
  conn: Connection;
  onClose: () => void;
  onDone: () => void;
}

export function createFlushDialogModel(props: FlushDialogProps) {
  const [typed, setTyped] = createSignal("");
  const [all, setAll] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const ready = () => typed() === props.conn.label && !busy();

  const run = async () => {
    if (!ready()) return;
    setBusy(true);
    setError(null);
    try {
      await api.flush(props.conn.id, all(), typed());
      props.onDone();
      props.onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return {
    typed,
    all,
    error,
    ready,
    setTyped,
    setAll,
    title: () => (all() ? "Flush every database" : `Flush database ${props.conn.db}`),
    runLabel: () => (busy() ? "Flushing…" : all() ? "Flush every database" : "Flush this database"),
    command: () => (all() ? "FLUSHALL" : "FLUSHDB"),
    scope: () => (all() ? "in every logical database" : `in database ${props.conn.db}`),
    run: () => void run(),
    focusLater: (el: HTMLElement) => setTimeout(() => el.focus()),
  };
}
