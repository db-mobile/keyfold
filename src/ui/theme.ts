// Light, dark or system, plus one of the nine Adwaita accents. The choice
// lives in localStorage and is applied before the first render; the native
// title bar follows through the window theme.
//
// "System" asks the backend, not the webview: on GNOME the dark style does
// not change the GTK 3 theme, so `prefers-color-scheme` still says light.

import { createSignal } from "solid-js";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

export type ThemeChoice = "system" | "light" | "dark";

export const ACCENTS = [
  { id: "blue", color: "#3584e4" },
  { id: "teal", color: "#2190a4" },
  { id: "green", color: "#3a944a" },
  { id: "yellow", color: "#c88800" },
  { id: "orange", color: "#ed5b00" },
  { id: "red", color: "#e62d42" },
  { id: "pink", color: "#d56199" },
  { id: "purple", color: "#9141ac" },
  { id: "indigo", color: "#5261c9" },
] as const;

export type Accent = (typeof ACCENTS)[number]["id"];

const THEME_KEY = "keyfold.theme";
const ACCENT_KEY = "keyfold.accent";
const SYSTEM_KEY = "keyfold.systemScheme";
const DEFAULT_ACCENT: Accent = "teal";

function stored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function store(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // the choice then lasts for this run only
  }
}

const initialTheme = (): ThemeChoice => {
  const t = stored(THEME_KEY);
  return t === "light" || t === "dark" ? t : "system";
};
const initialAccent = (): Accent => {
  const a = stored(ACCENT_KEY);
  return ACCENTS.some((x) => x.id === a) ? (a as Accent) : DEFAULT_ACCENT;
};

const [theme, setThemeSignal] = createSignal<ThemeChoice>(initialTheme());
const [accent, setAccentSignal] = createSignal<Accent>(initialAccent());

export { theme, accent };

type Scheme = "light" | "dark";

const systemDark = window.matchMedia("(prefers-color-scheme: dark)");
/// What the desktop said last, remembered so the next start does not flash.
let desktop: Scheme | null = (() => {
  const s = stored(SYSTEM_KEY);
  return s === "dark" || s === "light" ? s : null;
})();

const systemScheme = (): Scheme => desktop ?? (systemDark.matches ? "dark" : "light");

function setDesktop(s: unknown) {
  if (s !== "dark" && s !== "light") return;
  desktop = s;
  store(SYSTEM_KEY, s);
  if (theme() === "system") apply();
}

function apply() {
  const choice = theme();
  const resolved = choice === "system" ? systemScheme() : choice;
  const root = document.documentElement;
  root.dataset.theme = resolved;
  root.dataset.accent = accent();
  try {
    // always explicit: left to itself the title bar follows the GTK theme,
    // which can disagree with the desktop's dark style
    void getCurrentWindow()
      .setTheme(resolved)
      .catch(() => {});
  } catch {
    // not running inside Tauri (plain Vite preview)
  }
}

export function setTheme(next: ThemeChoice) {
  setThemeSignal(next);
  store(THEME_KEY, next);
  apply();
}

export function setAccent(next: Accent) {
  setAccentSignal(next);
  store(ACCENT_KEY, next);
  apply();
}

export function initTheme() {
  apply();
  systemDark.addEventListener("change", () => {
    if (theme() === "system" && desktop === null) apply();
  });
  try {
    void invoke<string | null>("system_color_scheme").then(setDesktop, () => {});
    void listen<string>("theme:system", (e) => setDesktop(e.payload)).catch(() => {});
  } catch {
    // not running inside Tauri
  }
}
