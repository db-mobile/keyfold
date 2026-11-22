// The header's database selector, driven as a user would, against a fake
// backend. Elements are found by role and label only.

import { render, screen, waitFor, within } from "@solidjs/testing-library";
import userEvent from "@testing-library/user-event";
import { createSignal } from "solid-js";
import { describe, expect, it, vi } from "vitest";
import type { Connection, Databases } from "./api";
import DialogHost from "./dialog";
import Header from "./Header";
import { createScanModel } from "./scanModel";
import { fakeApi } from "./test/fakeApi";
import { connection, databases } from "./test/fixtures";

function header(dbs: Databases | "denied", over: Partial<Connection> = {}) {
  const api = fakeApi({
    databases: vi.fn(() => (dbs === "denied" ? Promise.reject("NOPERM") : Promise.resolve(dbs))),
    memoryStats: () => Promise.resolve({ backendRss: 1 << 26, webviewRss: 1 << 27, processes: [] }),
  });
  const [conn, setConn] = createSignal(connection({ id: 1, db: 0, dbsize: 1500, ...over }));
  // what the app does: a switch opens a new session on the other database
  const onSwitchDb = vi.fn((db: number) => {
    setConn(connection({ ...conn(), id: conn().id + 1, db, dbsize: 0 }));
    return Promise.resolve();
  });
  const user = userEvent.setup();
  render(() => {
    const scan = createScanModel(conn, () => null, vi.fn(), vi.fn(() => Promise.resolve()));
    return (
      <>
        <Header
          conn={conn()}
          scan={scan}
          onDisconnect={vi.fn()}
          onSwitchDb={onSwitchDb}
          onFlush={vi.fn()}
          onRefresh={vi.fn()}
          onNewKey={vi.fn()}
          onJump={vi.fn()}
          onConsole={vi.fn()}
          consoleOpen={false}
          onPreferences={vi.fn()}
        />
        <DialogHost />
      </>
    );
  });
  const select = () => screen.getByRole<HTMLSelectElement>("combobox", { name: "Database" });
  const options = () => within(select()).getAllByRole<HTMLOptionElement>("option").map((o) => o.textContent);
  return { api, user, onSwitchDb, select, options };
}

describe("the database selector", () => {
  it("lists every database with its key count, the open one by the header's own count", async () => {
    const { api, select, options } = header(databases({ count: 4, used: [{ db: 0, keys: 1400 }, { db: 2, keys: 7 }] }));
    await waitFor(() => expect(options()).toHaveLength(4));
    expect(api.databases).toHaveBeenCalledWith(1);
    expect(options()).toEqual(["db 0, 1,500 keys", "db 1, empty", "db 2, 7 keys", "db 3, empty"]);
    expect(select()).toHaveValue("0");
    expect(select()).toBeEnabled();
  });

  it("switches once when another database is chosen, and reads the counts again", async () => {
    const { api, user, onSwitchDb, select, options } = header(databases({ count: 4, used: [{ db: 2, keys: 7 }] }));
    await waitFor(() => expect(options()).toHaveLength(4));
    await user.selectOptions(select(), "2");
    expect(onSwitchDb.mock.calls).toEqual([[2]]);
    await waitFor(() => expect(api.databases).toHaveBeenLastCalledWith(2));
    expect(select()).toHaveValue("2");
  });

  it("does nothing when the open database is chosen again", async () => {
    const { user, onSwitchDb, select, options } = header(databases({ count: 4 }));
    await waitFor(() => expect(options()).toHaveLength(4));
    await user.selectOptions(select(), "0");
    expect(onSwitchDb).not.toHaveBeenCalled();
  });

  it("goes back to the open database and says why when the server refuses", async () => {
    const { user, onSwitchDb, select, options } = header(databases({ count: 4 }));
    onSwitchDb.mockRejectedValueOnce("ERR DB index is out of range");
    await waitFor(() => expect(options()).toHaveLength(4));
    await user.selectOptions(select(), "3");

    const dialog = await screen.findByRole("alertdialog");
    expect(within(dialog).getByRole("heading", { name: "Database 3" })).toBeInTheDocument();
    expect(dialog).toHaveTextContent("ERR DB index is out of range");
    expect(select()).toHaveValue("0");
    await user.click(within(dialog).getByRole("button", { name: "OK" }));
    await waitFor(() => expect(select()).toBeEnabled());
  });

  it("is disabled on a server with one database", async () => {
    const { select, options } = header(databases({ count: 1 }));
    await waitFor(() => expect(select()).toHaveAttribute("title", "This server has one logical database"));
    expect(options()).toEqual(["db 0, 1,500 keys"]);
    expect(select()).toBeDisabled();
  });

  it("still shows the open database when the list cannot be read", async () => {
    const { api, select, options } = header("denied", { db: 3 });
    await waitFor(() => expect(api.databases).toHaveBeenCalled());
    expect(options()).toEqual(["db 0, empty", "db 1, empty", "db 2, empty", "db 3, 1,500 keys"]);
    expect(select()).toHaveValue("3");
  });

  it("reads the counts again when the selector gets the focus", async () => {
    const { api, user, select, options } = header(databases({ count: 2 }));
    await waitFor(() => expect(options()).toHaveLength(2));
    await user.click(select());
    await waitFor(() => expect(api.databases).toHaveBeenCalledTimes(2));
  });

  it("says when the number of databases is an assumption", async () => {
    const { select } = header(databases({ count: 16, configured: false }));
    await waitFor(() => expect(select().title).toContain("16 assumed"));
  });
});
