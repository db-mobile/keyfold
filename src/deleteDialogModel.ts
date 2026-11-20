// State and actions of the pattern delete (CONCEPT section 8): the preview
// from the index, the progress events of the batched UNLINK and its pause,
// resume and cancel. The markup is in `DeleteDialog.tsx`.

import { createSignal, onCleanup, onMount } from "solid-js";
import { api, fmtCount, type Connection, type DeletePreview, type DeleteProgress, type DeleteTarget } from "./api";
import { confirmWrite } from "./dialogModel";

export interface DeleteDialogProps {
  conn: Connection;
  target: DeleteTarget;
  onClose: () => void;
}

export function createDeleteDialogModel(props: DeleteDialogProps) {
  const [preview, setPreview] = createSignal<DeletePreview | null>(null);
  const [progress, setProgress] = createSignal<DeleteProgress | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [starting, setStarting] = createSignal(false);

  onMount(() => {
    void api
      .deletePreview(props.conn.id, props.target)
      .then(setPreview)
      .catch((e) => setError(String(e)));
    let unlisten: (() => void) | undefined;
    void api
      .onDeleteProgress((p) => {
        if (p.session === props.conn.id && p.query === props.target.query) setProgress(p);
      })
      .then((u) => (unlisten = u));
    onCleanup(() => unlisten?.());
  });

  const running = () => {
    const ph = progress()?.phase;
    return starting() || ph === "counting" || ph === "deleting" || ph === "paused";
  };
  const finished = () => {
    const ph = progress()?.phase;
    return ph === "complete" || ph === "cancelled" || ph === "failed";
  };

  const start = async () => {
    const p = preview();
    if (!p) return;
    if (!(await confirmWrite(props.conn, `UNLINK ${fmtCount(p.count)} keys matching ${props.target.query}`))) return;
    setStarting(true);
    setError(null);
    try {
      await api.deleteStart(props.conn.id, props.target, true);
    } catch (e) {
      setError(String(e));
      setStarting(false);
    }
  };

  return {
    preview,
    progress,
    error,
    running,
    finished,
    /// Nothing started yet: the preview and the confirming button show.
    idle: () => !running() && !finished(),
    paused: () => progress()?.phase === "paused",
    counting: () => !preview() && !error(),
    canDelete: () => !!preview() && preview()!.count !== 0,
    deleteCount: () => (preview() ? fmtCount(preview()!.count) : ""),
    start: () => start(),
    pause: () => api.deletePause(props.conn.id),
    resume: () => api.deleteResume(props.conn.id),
    cancel: () => api.deleteCancel(props.conn.id),
    /// A running delete cannot be dismissed, only stopped.
    close: () => !running() && props.onClose(),

    count: (p: DeletePreview) => fmtCount(p.count),
    noun: (p: DeletePreview) => (p.count === 1 ? "key" : "keys"),
    took: (p: DeletePreview) => p.tookMs.toFixed(0),
    how: (p: DeletePreview) => (p.count > 0 ? " Deleted with batched UNLINK, 500 keys per call; the index is updated as each call returns." : ""),
    /// An on-demand index knows only part of the keyspace outside its scanned subtrees.
    partialNote: () =>
      props.conn.indexMode === "onDemand" && props.conn.index.fullAt === null
        ? "The index is on demand: keys it has not seen are not counted and not deleted. Scan the subtree first to delete all of it."
        : "",
    hasSample: (p: DeletePreview) => p.sample.length > 0,
    hasMore: (p: DeletePreview) => p.count > p.sample.length,
    more: (p: DeletePreview) => fmtCount(p.count - p.sample.length),

    barWidth: (pr: DeleteProgress) => `${pr.total ? Math.min(100, (pr.deleted / Math.max(1, pr.total!)) * 100) : 0}%`,
    deleted: (pr: DeleteProgress) => fmtCount(pr.deleted),
    total: (pr: DeleteProgress) => (pr.total === null ? "?" : fmtCount(pr.total!)),
    removed: (pr: DeleteProgress) => fmtCount(pr.removed),
    seconds: (pr: DeleteProgress) => (pr.elapsedMs / 1000).toFixed(1),
    failure: (pr: DeleteProgress) => (pr.error ? `. ${pr.error}` : ""),
  };
}
