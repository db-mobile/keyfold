// A key name as it crosses the IPC (see `src-tauri/src/wire.rs`). Redis
// names are byte strings; a JS string only holds the ones that are valid
// UTF-8. For any other name `text` is an escaped form for display and `raw`
// carries the exact bytes as hex, which goes back to the backend unchanged.
// Logic only: identity and comparison of key references.

import type { KeyRef } from "./api";

/// A name the user typed, or one known to be text.
export const textKey = (text: string): KeyRef => ({ text, raw: null });

/// A string that is equal for two references exactly when they name the
/// same key. Usable as a store or map key; the prefix keeps a text name
/// from colliding with the hex of a binary one.
export const keyId = (k: KeyRef): string => (k.raw !== null ? `x:${k.raw}` : `t:${k.text}`);

export const sameKey = (a: KeyRef | null | undefined, b: KeyRef | null | undefined): boolean => !!a && !!b && keyId(a) === keyId(b);

/// The name is not valid UTF-8: its text is an escaped rendering.
export const isBinary = (k: KeyRef | null | undefined): boolean => !!k && k.raw !== null;

/// A literal as a glob that matches only itself (the backend's `glob::escape`).
export const globEscape = (literal: string): string => literal.replace(/[*?[\]\\]/g, "\\$&");
