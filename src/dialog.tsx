// The host that renders the confirmation in flight. Markup only; `ask`,
// `confirmWrite` and the state are in `dialogModel.ts`.

import { Show } from "solid-js";
import { createDialogHostModel } from "./dialogModel";
import Dialog from "./ui/Dialog";

export { ask, confirmWrite, type Ask } from "./dialogModel";

export default function DialogHost() {
  const m = createDialogHostModel();
  return (
    <Show when={m.asked()}>
      {(a) => (
        <Dialog
          alert
          title={a().title}
          onClose={m.cancel}
          onEnter={m.confirmIfReady}
          footer={
            <>
              <button class="btn" onClick={m.cancel}>
                Cancel
              </button>
              <button class={a().danger ? "btn btn-danger" : "btn btn-primary"} disabled={!m.ready()} onClick={m.confirm} ref={m.focusConfirm}>
                {a().confirm}
              </button>
            </>
          }
        >
          <Show when={a().body}>
            <p>{a().body}</p>
          </Show>
          <Show when={a().typeToConfirm}>
            <label class="form-group type-to-confirm">
              <span class="form-label">
                Type <code>{a().typeToConfirm}</code> to confirm
              </span>
              <input class="entry monospace" ref={m.focusLater} value={m.typed()} onInput={(e) => m.setTyped(e.currentTarget.value)} spellcheck={false} />
            </label>
          </Show>
        </Dialog>
      )}
    </Show>
  );
}
