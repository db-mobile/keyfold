// The host that renders the toasts in flight. Markup only; the queue and
// `toast()` are in `toastModel.ts`.

import { For } from "solid-js";
import { dismissToast, toasts } from "./toastModel";

export { toast } from "./toastModel";

export default function ToastHost() {
  return (
    <div class="toast-container" aria-live="polite">
      <For each={toasts()}>
        {(t) => (
          <div
            class="toast"
            classList={{ "toast--visible": t.visible, "toast--error": t.kind === "error", "toast--success": t.kind === "success" }}
            role={t.kind === "error" ? "alert" : "status"}
            onClick={() => dismissToast(t.id)}
          >
            <span class="toast__message">{t.message}</span>
          </div>
        )}
      </For>
    </div>
  );
}
