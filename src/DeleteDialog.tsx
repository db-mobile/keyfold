// Pattern delete (CONCEPT section 8): the exact count and a 20-key sample
// from the index before confirming, then batched UNLINK in the background
// with pause and cancel, the index updated as it goes. The keys come from
// the index, so a stale index deletes what it knows; "verify against
// server" in the search header reconciles first when that matters.
// Markup only; state and actions are in `deleteDialogModel.ts`.

import { For, Show } from "solid-js";
import { createDeleteDialogModel, type DeleteDialogProps } from "./deleteDialogModel";
import Dialog from "./ui/Dialog";

export default function DeleteDialog(props: DeleteDialogProps) {
  const m = createDeleteDialogModel(props);
  return (
    <Dialog
      size="lg"
      persistent={m.running()}
      title={
        <>
          Delete keys matching <code>{props.target.query}</code>
        </>
      }
      onClose={m.close}
      footer={
        <>
          <Show when={m.idle()}>
            <button class="btn" onClick={props.onClose}>
              Cancel
            </button>
            <button class="btn btn-danger" disabled={!m.canDelete()} onClick={m.start}>
              Delete {m.deleteCount()} keys
            </button>
          </Show>
          <Show when={m.running()}>
            <Show
              when={m.paused()}
              fallback={
                <button class="btn" onClick={m.pause}>
                  Pause
                </button>
              }
            >
              <button class="btn" onClick={m.resume}>
                Resume
              </button>
            </Show>
            <button class="btn btn-danger" onClick={m.cancel}>
              Stop
            </button>
          </Show>
          <Show when={m.finished()}>
            <button class="btn" onClick={props.onClose}>
              Close
            </button>
          </Show>
        </>
      }
    >
      <div class="form-stack">
        <Show when={m.counting()}>
          <p class="dim-label">Counting…</p>
        </Show>
        <Show when={m.preview()}>
          {(p) => (
            <>
              <p>
                <b class="numeric">{m.count(p())}</b> {m.noun(p())} in the index match ({p().mode}, counted in {m.took(p())} ms).
                {m.how(p())}
              </p>
              <Show when={m.partialNote()}>
                <p class="warning-text">{m.partialNote()}</p>
              </Show>
              <Show when={m.hasSample(p())}>
                <div class="key-sample">
                  <For each={p().sample}>{(k) => <div>{k}</div>}</For>
                  <Show when={m.hasMore(p())}>
                    <div class="dim-label">… and {m.more(p())} more</div>
                  </Show>
                </div>
              </Show>
            </>
          )}
        </Show>
        <Show when={m.progress()}>
          {(pr) => (
            <>
              <div class="progress">
                <div class="bar" style={{ width: m.barWidth(pr()) }} />
              </div>
              <p class="numeric">
                {pr().phase}: {m.deleted(pr())} of {m.total(pr())} deleted on the server, {m.removed(pr())} removed from the index,{" "}
                {pr().calls} calls, {m.seconds(pr())} s{m.failure(pr())}
              </p>
            </>
          )}
        </Show>
        <Show when={m.error()}>
          <p class="error-text" role="alert">
            {m.error()}
          </p>
        </Show>
      </div>
    </Dialog>
  );
}
