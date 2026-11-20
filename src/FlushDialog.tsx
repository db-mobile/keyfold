// FLUSHDB / FLUSHALL require typing the connection's name (CONCEPT 8);
// the backend checks the name again. Markup only; state and actions are
// in `flushDialogModel.ts`.

import { Show } from "solid-js";
import { createFlushDialogModel, type FlushDialogProps } from "./flushDialogModel";
import Dialog from "./ui/Dialog";

export default function FlushDialog(props: FlushDialogProps) {
  const m = createFlushDialogModel(props);
  return (
    <Dialog
      title={m.title()}
      onClose={props.onClose}
      onEnter={m.run}
      footer={
        <>
          <button class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-danger" disabled={!m.ready()} onClick={m.run}>
            {m.runLabel()}
          </button>
        </>
      }
    >
      <div class="form-stack">
        <p>
          {m.command()} deletes every key {m.scope()} of <b>{props.conn.label}</b>. The index is cleared with it.
        </p>
        <div class="boxed-list">
          <label class="row">
            <div class="row-content">
              <span class="title">All logical databases</span>
              <span class="subtitle">Runs FLUSHALL instead of FLUSHDB</span>
            </div>
            <div class="suffix">
              <input type="checkbox" class="switch" role="switch" checked={m.all()} onChange={(e) => m.setAll(e.currentTarget.checked)} />
            </div>
          </label>
        </div>
        <label class="form-group">
          <span class="form-label">
            Type <code>{props.conn.label}</code> to confirm
          </span>
          <input class="entry monospace" ref={m.focusLater} value={m.typed()} onInput={(e) => m.setTyped(e.currentTarget.value)} spellcheck={false} />
        </label>
        <Show when={m.error()}>
          <p class="error-text" role="alert">
            {m.error()}
          </p>
        </Show>
      </div>
    </Dialog>
  );
}
