# UI style guide

Keyfold looks like a GNOME application. The style is ported from Resonance
(`~/VSCodeProjects/resonance`, `docs/UI_STYLE_GUIDE.md` there): the Adwaita
palette, libadwaita widget names, and Adwaita symbolic icons, as plain CSS.
Build on the shared widgets and tokens in `src/styles/`; feature styles
describe layout and intentional exceptions, they do not restyle controls.

## Where things live

| Path | Holds |
|---|---|
| `src/styles/tokens.css` | Sizes, spacing, radii, light palette, accent pairs |
| `src/styles/themes.css` | Dark palette (`:root[data-theme="dark"]`) and per-theme accent text colours |
| `src/styles/widgets/` | Buttons, entries, selects, switches, boxed lists, popovers, dialogs, toasts, badges, banner, progress |
| `src/styles/layout/` | App shell, sidebar, splitter |
| `src/styles/features/` | Header bar, tree, key list, value view, connections, palette, preferences |
| `src/ui/` | `Icon`, `Popover`, `Dialog`, `Toast`, `Select`, `Splitter`, `theme.ts`, `density.ts` |
| `src/assets/icons/` | Adwaita symbolic SVGs (LGPL v3 or CC BY-SA 3.0, see `LICENSE` there) |

## Rules

- **Colour comes from tokens.** No hex values in feature styles or components.
  `--accent-bg-color` / `--accent-fg-color` fill widgets; `--accent-color` is
  for text and icons on a regular surface.
- **Environment colours are not the accent.** `prod` is the destructive red,
  `staging` the warning amber, `dev` neutral, in every theme and with every
  accent. A prod connection draws a 2 px red line across the top of the header
  bar. Nothing else in the window competes with it.
- **Surfaces:** header bar (`--headerbar-bg-color`), sidebar
  (`--sidebar-bg-color`), value view (`--view-bg-color`), cards and boxed
  lists (`--card-bg-color` with `--shadow-card`).
- **Controls** are 34 px high, compact ones 28 px, embedded ones 24 px. Icons
  are 16 px, disclosure arrows 12 px. Icon-only buttons are square and carry
  `title` and `aria-label`.
- **Buttons:** `suggested-action` / `btn-primary` for the one main action,
  plain for secondary, `flat` for utilities, `destructive-action` /
  `btn-danger` for what deletes.
- **Radii:** controls small, cards medium, dialogs large, badges pill.
- **Spacing:** 6 px between adjacent controls, 12 px inside panels, 18 px
  between sections.
- **Type:** the system UI font at 14 px for chrome. Key names, values and
  anything typed into Redis use `--font-mono` at 13 px; counts and durations
  use tabular numerals. Nothing is bundled.
- **Names that are not UTF-8** show the backend's escaped text (`\xNN`) in
  the same mono face, followed by a `badge mark` reading "binary". Never
  build a key from that text: the exact bytes travel beside it (`KeyRef`).
- **Data rows are denser than chrome.** Their heights live in
  `src/ui/density.ts` and reach CSS as `--tree-row-height`,
  `--list-row-height` and `--value-row-height`. Change them there only: the
  virtualisers and the CSS read the same numbers.
- **Icons** are CSS masks (`<Icon name="…" />`), so they take the text colour.
  To add one, copy the symbolic SVG into `src/assets/icons/`, add its class to
  `src/styles/icons.css` and its name to `IconName`.
- **Dialogs** go through `src/ui/Dialog.tsx` (focus stays inside, returns on
  close, Escape closes). Confirmations use `ask()` from `src/dialog.tsx`, which
  renders the alert layout. Forms that hold typed input are `persistent`.
- **Popovers and menus** go through `src/ui/Popover.tsx`. A container that
  holds one must not clip (`overflow: visible`).
- **Feedback:** the result of a write is a toast (`toast()`); what the user
  must know about the connection is a banner under the header bar; how fresh
  the index is sits in the sidebar footer.
- **Partial knowledge is marked, not hidden.** A count the index cannot
  vouch for carries "≥"; in an on-demand index it is also set in italics
  and its tooltip says when the subtree was scanned, or that it was not.
  A scan that runs or waits for a node shows as a `badge mark accent` on
  that row only. An action that makes the server walk every key says so
  where it is offered, and asks first on prod.
- **The console** is the one panel below the panes: mono at the value row
  height, the typed line bold, errors in the error colour, and one dim line
  of facts (node, time, what the index did) after each reply. It never
  shows a dialog of its own; confirmations go through `ask()`.
- **Wording:** sentence case, plain verbs, the same name for an action in the
  button, the dialog and the toast. Errors say what happened.
- **Motion:** 100 ms for hover and disclosure, 200 ms for switches and toasts.
  No entrance animations. `prefers-reduced-motion` is honoured globally.
- **Focus** must stay visible, including on actions revealed by hover.
- **Markup and logic live in separate files.** A component is a markup file
  `Name.tsx` and a model file `nameModel.ts`. The model exports
  `createNameModel(props)` and the props type; it holds every signal, memo,
  effect, backend call and event handler, and contains no JSX. The markup file
  calls the model once (`const m = createNameModel(props)`) and returns JSX
  that reads `m.*` and `props.*`. In a markup file an inline handler may only
  pass an event's value on (`onInput={(e) => m.setName(e.currentTarget.value)}`);
  conditions, formatting and anything with a branch belong in the model.
  Markup is never built from strings or with `document.createElement`, and
  `innerHTML` is not used. Inline `style` carries computed geometry only.
  This is Keyfold's form of Resonance's rule that script does not inject
  markup; Resonance keeps its markup in `.html` templates.

## Deviations from Resonance

- Dark `--shade-color` is white at 8 % (as in libadwaita) instead of
  Resonance's black at 25 %, so buttons and hovers lighten on dark surfaces.
- Dark mode is selected by `data-theme` on `<html>`, not by swapping a
  stylesheet.
- "Adwaita Sans" and "Adwaita Mono" lead the font stacks.
- Added widgets: `banner`, `progress`.
- Markup is JSX in `.tsx` files next to a model file, not `.html` templates
  cloned by a loader: Solid compiles JSX, it cannot bind an HTML file.
