// Row heights of the virtualised panes. The virtualisers and the CSS both
// read these, so the two cannot drift apart.

export const TREE_ROW = 28;
export const LIST_ROW = 28;
export const VALUE_ROW = 24;

export function applyDensity() {
  const s = document.documentElement.style;
  s.setProperty("--tree-row-height", `${TREE_ROW}px`);
  s.setProperty("--list-row-height", `${LIST_ROW}px`);
  s.setProperty("--value-row-height", `${VALUE_ROW}px`);
}
