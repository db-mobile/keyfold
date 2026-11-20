// Create a key: the first write of its type creates it on the server and
// write-through puts it in the index. Markup only; state and actions are
// in `newKeyDialogModel.ts`.

import { Match, Show, Switch } from "solid-js";
import { createNewKeyDialogModel, type NewKeyDialogProps } from "./newKeyDialogModel";
import Dialog from "./ui/Dialog";
import Select from "./ui/Select";

export default function NewKeyDialog(props: NewKeyDialogProps) {
  const m = createNewKeyDialogModel(props);
  return (
    <Dialog
      title="New key"
      persistent
      onClose={props.onClose}
      onEnter={m.create}
      footer={
        <>
          <button class="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button class="btn btn-primary" disabled={!m.ready()} onClick={m.create}>
            {m.createLabel()}
          </button>
        </>
      }
    >
      <div class="form-stack">
        <div class="form-row">
          <label class="form-group grow">
            <span class="form-label">Name</span>
            <input class="entry monospace" ref={m.focusLater} value={m.name()} onInput={(e) => m.setName(e.currentTarget.value)} spellcheck={false} />
          </label>
          <label class="form-group">
            <span class="form-label">Type</span>
            <Select value={m.type()} onChange={(e) => m.setType(e.currentTarget.value)}>
              <option value="string">String</option>
              <option value="hash">Hash</option>
              <option value="set">Set</option>
              <option value="zset">Sorted set</option>
              <option value="list">List</option>
              <option value="stream">Stream</option>
            </Select>
          </label>
        </div>
        <Switch>
          <Match when={m.isString()}>
            <label class="form-group">
              <span class="form-label">Value</span>
              <textarea class="entry monospace" rows={4} value={m.a()} onInput={(e) => m.setA(e.currentTarget.value)} spellcheck={false} />
            </label>
          </Match>
          <Match when={m.isFieldValue()}>
            <label class="form-group">
              <span class="form-label">Field</span>
              <input class="entry monospace" value={m.a()} onInput={(e) => m.setA(e.currentTarget.value)} spellcheck={false} />
            </label>
            <label class="form-group">
              <span class="form-label">Value</span>
              <input class="entry monospace" value={m.b()} onInput={(e) => m.setB(e.currentTarget.value)} spellcheck={false} />
            </label>
          </Match>
          <Match when={m.isSingle()}>
            <label class="form-group">
              <span class="form-label">{m.singleLabel()}</span>
              <input class="entry monospace" value={m.a()} onInput={(e) => m.setA(e.currentTarget.value)} spellcheck={false} />
            </label>
          </Match>
          <Match when={m.isZset()}>
            <label class="form-group">
              <span class="form-label">Member</span>
              <input class="entry monospace" value={m.a()} onInput={(e) => m.setA(e.currentTarget.value)} spellcheck={false} />
            </label>
            <label class="form-group">
              <span class="form-label">Score</span>
              <input class="entry monospace" value={m.b()} onInput={(e) => m.setB(e.currentTarget.value)} placeholder="0" spellcheck={false} />
            </label>
          </Match>
        </Switch>
        <Show when={m.error()}>
          <p class="error-text" role="alert">
            {m.error()}
          </p>
        </Show>
      </div>
    </Dialog>
  );
}
