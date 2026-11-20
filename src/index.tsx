/* @refresh reload */
import { render } from "solid-js/web";
import App from "./App";
import "./styles/index.css";
import { applyDensity } from "./ui/density";
import { initTheme } from "./ui/theme";
import { api } from "./api";

window.addEventListener("error", (e) => void api.log("error", `${e.message} (${e.filename}:${e.lineno})`));
window.addEventListener("unhandledrejection", (e) => void api.log("error", `unhandled rejection: ${String(e.reason)}`));
initTheme();
applyDensity();
void api.log("info", "frontend loaded");

render(() => <App />, document.getElementById("root") as HTMLElement);
