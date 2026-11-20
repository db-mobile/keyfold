// The shared modal: scrim, centred title, optional close button, body and
// a footer of actions. `alert` gives the compact confirmation layout with
// a divided row of actions. Markup only; behaviour is in `dialogModel.ts`.

import { Show } from "solid-js";
import { createDialogModel, type DialogProps } from "./dialogModel";
import Icon from "./Icon";

export default function Dialog(props: DialogProps) {
  const m = createDialogModel(props);
  return (
    <div class="modal-overlay" onMouseDown={m.onScrimDown}>
      <div
        ref={m.setEl}
        class={`modal-dialog ${props.class ?? ""}`}
        classList={{ "alert-dialog": props.alert, "modal-dialog--sm": props.size === "sm", "modal-dialog--lg": props.size === "lg" }}
        role={props.alert ? "alertdialog" : "dialog"}
        aria-modal="true"
        tabindex="-1"
        onKeyDown={m.onKeyDown}
      >
        <div class="dialog-header">
          <h2 class="dialog-title">{props.title}</h2>
          <Show when={!props.alert}>
            <button class="dialog-close-btn" onClick={props.onClose} aria-label="Close" title="Close">
              <Icon name="x" />
            </button>
          </Show>
        </div>
        <Show when={props.children}>
          <div class="dialog-body">{props.children}</div>
        </Show>
        <Show when={props.footer}>
          <div class="dialog-footer">{props.footer}</div>
        </Show>
      </div>
    </div>
  );
}
