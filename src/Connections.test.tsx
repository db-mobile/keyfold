// The connection manager, clicked through as a user would, against a fake
// backend. Elements are found by role and label only.

import { render, screen, waitFor, within } from "@solidjs/testing-library";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { OrphanIndex, ProfileInput, StoredProfile } from "./api";
import Connections from "./Connections";
import DialogHost from "./dialog";
import { fakeApi } from "./test/fakeApi";
import { storedProfile } from "./test/fixtures";

const MB = 1 << 20;

/// A backend that keeps stored connections in a list, as `profiles.json` does.
function manager(initial: StoredProfile[] = [], orphans: OrphanIndex[] = []) {
  let stored = [...initial];
  let left = [...orphans];
  const api = fakeApi({
    profilesList: () => Promise.resolve([...stored]),
    indexesOrphans: () => Promise.resolve([...left]),
    profileSave: vi.fn((input: ProfileInput, password: string | null) => {
      const before = stored.find((p) => p.id === input.id);
      const saved = { ...input, id: input.id ?? "new-id", passwordSaved: password ? true : password === "" ? false : (before?.passwordSaved ?? false), sshSecretSaved: false };
      stored = [...stored.filter((p) => p.id !== saved.id), { ...saved, indexBytes: before?.indexBytes ?? 0 }];
      return Promise.resolve(saved);
    }),
    profileDelete: vi.fn((id: string) => {
      stored = stored.filter((p) => p.id !== id);
      return Promise.resolve();
    }),
    profileClearIndex: vi.fn((id: string) => {
      stored = stored.map((p) => (p.id === id ? { ...p, indexBytes: 0 } : p));
      return Promise.resolve();
    }),
    indexOrphanClear: vi.fn((name: string) => {
      left = left.filter((o) => o.name !== name);
      return Promise.resolve();
    }),
    profileFromUrl: vi.fn(() => Promise.resolve({ name: "cache.internal", host: "cache.internal", port: 6380, db: 2, username: "app", password: "s3cret", tls: true, tlsInsecure: false })),
  });
  const onConnect = vi.fn(() => Promise.resolve());
  const onConnectProfile = vi.fn(() => Promise.resolve());
  const onPreferences = vi.fn();
  const user = userEvent.setup();
  render(() => (
    <>
      <Connections onConnect={onConnect} onConnectProfile={onConnectProfile} error={null} onPreferences={onPreferences} />
      <DialogHost />
    </>
  ));
  return { api, user, onConnect, onConnectProfile, onPreferences };
}

const editor = () => screen.findByRole("dialog");
const field = (dialog: HTMLElement, label: string | RegExp) => within(dialog).getByLabelText(label);
const openMenu = async (user: ReturnType<typeof userEvent.setup>, name: string) => {
  await user.click(await screen.findByRole("button", { name: `More for ${name}` }));
  return screen.getByRole("menu");
};

describe("an empty manager", () => {
  it("invites to add a connection and opens the editor with the defaults", async () => {
    const { user } = manager();
    expect(await screen.findByText("No connections yet")).toBeInTheDocument();
    await user.click(screen.getAllByRole("button", { name: "New connection" })[0]);

    const dialog = await editor();
    expect(within(dialog).getByRole("heading", { name: "New connection" })).toBeInTheDocument();
    expect(field(dialog, "Name")).toHaveValue("");
    expect(field(dialog, "Host")).toHaveValue("127.0.0.1");
    expect(field(dialog, "Port")).toHaveValue(6379);
    expect(field(dialog, "Database")).toHaveValue(0);
    expect(field(dialog, /^Environment/)).toHaveValue("dev");
    expect(field(dialog, "SSH host")).toBeDisabled();
  });
});

describe("the editor", () => {
  it("saves what was typed and sends no password when none was typed", async () => {
    const { api, user, onConnectProfile } = manager();
    await user.click((await screen.findAllByRole("button", { name: "New connection" }))[0]);
    const dialog = await editor();

    await user.type(field(dialog, "Name"), "Orders");
    await user.clear(field(dialog, "Host"));
    await user.type(field(dialog, "Host"), "orders.internal");
    await user.clear(field(dialog, "Port"));
    await user.type(field(dialog, "Port"), "6400");
    await user.selectOptions(field(dialog, /^Environment/), "prod");
    await user.selectOptions(field(dialog, /^Scan from/), "replica");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(api.profileSave).toHaveBeenCalledTimes(1);
    const [input, password, sshSecret] = api.profileSave.mock.calls[0];
    expect(input).toEqual({
      id: null,
      name: "Orders",
      host: "orders.internal",
      port: 6400,
      db: 0,
      username: null,
      tls: false,
      tlsInsecure: false,
      tag: "prod",
      scanProfile: null,
      scanTarget: "replica",
      ephemeralIndex: null,
      indexMode: null,
      ssh: null,
    });
    expect([password, sshSecret]).toEqual([null, null]);
    expect(onConnectProfile).not.toHaveBeenCalled();

    // the list shows the new connection
    expect(await screen.findByText("Orders")).toBeInTheDocument();
    expect(screen.getByText("orders.internal:6400/0")).toBeInTheDocument();
  });

  it("fills the fields from a URL, password included", async () => {
    const { api, user } = manager();
    await user.click((await screen.findAllByRole("button", { name: "New connection" }))[0]);
    const dialog = await editor();
    const fill = within(dialog).getByRole("button", { name: "Fill" });
    expect(fill).toBeDisabled();

    await user.type(field(dialog, "Fill from a URL"), "rediss://app:s3cret@cache.internal:6380/2");
    await user.click(fill);
    await waitFor(() => expect(field(dialog, "Host")).toHaveValue("cache.internal"));
    expect(api.profileFromUrl).toHaveBeenCalledWith("rediss://app:s3cret@cache.internal:6380/2");
    expect(field(dialog, "Name")).toHaveValue("cache.internal");
    expect(field(dialog, "Port")).toHaveValue(6380);
    expect(field(dialog, "Database")).toHaveValue(2);
    expect(field(dialog, "Username")).toHaveValue("app");
    expect(field(dialog, "Password")).toHaveValue("s3cret");
    expect(within(dialog).getByRole("switch", { name: "TLS" })).toBeChecked();
    expect(field(dialog, "Fill from a URL")).toHaveValue("");

    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.profileSave).toHaveBeenCalled());
    expect(api.profileSave.mock.calls[0][1]).toBe("s3cret");
  });

  it("connects after saving when asked to", async () => {
    const { user, onConnectProfile } = manager();
    await user.click((await screen.findAllByRole("button", { name: "New connection" }))[0]);
    const dialog = await editor();
    await user.type(field(dialog, "Name"), "Orders");
    await user.click(within(dialog).getByRole("button", { name: "Save and connect" }));
    await waitFor(() => expect(onConnectProfile).toHaveBeenCalledWith("new-id"));
  });

  it("stays open with the error when the backend refuses the save", async () => {
    const { api, user } = manager();
    api.profileSave.mockRejectedValueOnce("a connection needs a name");
    await user.click((await screen.findAllByRole("button", { name: "New connection" }))[0]);
    const dialog = await editor();
    await user.type(field(dialog, "Host"), "x");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent("a connection needs a name");
    expect(field(dialog, "Host")).toHaveValue("127.0.0.1x");
    expect(within(dialog).getByRole("button", { name: "Save" })).toBeEnabled();
  });

  it("discards the draft on Cancel", async () => {
    const { api, user } = manager();
    await user.click((await screen.findAllByRole("button", { name: "New connection" }))[0]);
    const dialog = await editor();
    await user.type(field(dialog, "Name"), "Orders");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(api.profileSave).not.toHaveBeenCalled();
  });

  it("edits a stored connection under its id and keeps its password unless told otherwise", async () => {
    const { api, user } = manager([storedProfile({ name: "Cache", passwordSaved: true })]);
    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Edit…" }));
    const dialog = await editor();
    expect(within(dialog).getByRole("heading", { name: "Edit Cache" })).toBeInTheDocument();
    expect(field(dialog, "Host")).toHaveValue("cache.internal");
    expect(field(dialog, "Password")).toHaveAttribute("placeholder", "Unchanged, in the OS keychain");

    await user.clear(field(dialog, "Name"));
    await user.type(field(dialog, "Name"), "Cache (old)");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.profileSave).toHaveBeenCalled());
    const [input, password] = api.profileSave.mock.calls[0];
    expect(input).toMatchObject({ id: "11111111-1111-4111-8111-111111111111", name: "Cache (old)" });
    expect(input).not.toHaveProperty("passwordSaved");
    expect(password).toBeNull();
  });

  it("removes the stored password only when that is ticked", async () => {
    const { api, user } = manager([storedProfile({ passwordSaved: true })]);
    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Edit…" }));
    const dialog = await editor();
    await user.click(within(dialog).getByRole("checkbox", { name: "Remove the stored password from the keychain" }));
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.profileSave).toHaveBeenCalled());
    expect(api.profileSave.mock.calls[0][1]).toBe("");
  });

  it("sends the tunnel only while it is switched on", async () => {
    const { api, user } = manager();
    await user.click((await screen.findAllByRole("button", { name: "New connection" }))[0]);
    const dialog = await editor();
    await user.click(within(dialog).getByRole("switch", { name: /^SSH tunnel/ }));
    await user.type(field(dialog, "SSH host"), "bastion.example");
    await user.type(field(dialog, "SSH user"), "ops");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.profileSave).toHaveBeenCalledTimes(1));
    expect(api.profileSave.mock.calls[0][0].ssh).toEqual({ host: "bastion.example", port: 22, user: "ops", auth: "agent", keyPath: null });
  });
});

describe("stored connections", () => {
  const two = () => [storedProfile({ indexBytes: 12 * MB }), storedProfile({ id: "22222222-2222-4222-8222-222222222222", name: "Sessions", host: "10.0.0.5", username: "app", tls: true, tag: "prod" })];

  it("lists each one with where it points and the size of its index", async () => {
    manager(two());
    expect(await screen.findByText("cache.internal:6379/0")).toBeInTheDocument();
    expect(screen.getByText("Index 12.0 MB")).toBeInTheDocument();
    expect(screen.getByText("app@10.0.0.5:6379/0, TLS")).toBeInTheDocument();
    expect(screen.getByText("No index")).toBeInTheDocument();
  });

  it("connects one and holds every other action while that runs", async () => {
    const { user, onConnectProfile } = manager(two());
    let finish = () => {};
    onConnectProfile.mockImplementationOnce(() => new Promise<void>((resolve) => (finish = resolve)));
    await screen.findByText("Cache");
    // the rows come first; the last Connect button is the one of "Connect by URL"
    await user.click(screen.getAllByRole("button", { name: "Connect" })[0]);

    expect(onConnectProfile).toHaveBeenCalledWith("11111111-1111-4111-8111-111111111111");
    expect(screen.getByRole("button", { name: "Connecting…" })).toBeDisabled();
    expect(screen.getAllByRole("button", { name: "Connect" })[0]).toBeDisabled();
    expect(screen.getByRole("button", { name: "More for Cache" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "More for Sessions" })).toBeDisabled();

    finish();
    await waitFor(() => expect(screen.getByRole("button", { name: "More for Cache" })).toBeEnabled());
  });

  it("deletes only after the confirmation", async () => {
    const { api, user } = manager(two());
    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Delete…" }));
    let confirm = await screen.findByRole("alertdialog");
    expect(within(confirm).getByRole("heading", { name: "Delete Cache?" })).toBeInTheDocument();
    expect(confirm).toHaveTextContent("its index (12.0 MB)");
    await user.click(within(confirm).getByRole("button", { name: "Cancel" }));
    expect(api.profileDelete).not.toHaveBeenCalled();
    expect(screen.getByText("Cache")).toBeInTheDocument();

    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Delete…" }));
    confirm = await screen.findByRole("alertdialog");
    await user.click(within(confirm).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(screen.queryByText("Cache")).not.toBeInTheDocument());
    expect(api.profileDelete).toHaveBeenCalledWith("11111111-1111-4111-8111-111111111111");
    expect(screen.getByText("Sessions")).toBeInTheDocument();
  });

  it("shows why a delete was refused", async () => {
    const { api, user } = manager(two());
    api.profileDelete.mockRejectedValueOnce("disconnect before you delete this connection");
    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Delete…" }));
    await user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Delete" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("disconnect before you delete this connection");
    expect(screen.getByText("Cache")).toBeInTheDocument();
  });

  it("shows why an index could not be cleared", async () => {
    const { api, user } = manager(two());
    api.profileClearIndex.mockRejectedValueOnce("permission denied");
    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Clear index…" }));
    await user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Clear index" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("permission denied");
  });

  it("clears an index after the confirmation, and offers that only where there is one", async () => {
    const { api, user } = manager(two());
    const without = await openMenu(user, "Sessions");
    expect(within(without).getByRole("menuitem", { name: "Clear index…" })).toBeDisabled();
    await user.keyboard("{Escape}");

    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Clear index…" }));
    let confirm = await screen.findByRole("alertdialog");
    await user.click(within(confirm).getByRole("button", { name: "Cancel" }));
    expect(api.profileClearIndex).not.toHaveBeenCalled();

    await user.click(within(await openMenu(user, "Cache")).getByRole("menuitem", { name: "Clear index…" }));
    confirm = await screen.findByRole("alertdialog");
    expect(confirm).toHaveTextContent("Deletes 12.0 MB on disk");
    await user.click(within(confirm).getByRole("button", { name: "Clear index" }));
    await waitFor(() => expect(screen.queryByText("Index 12.0 MB")).not.toBeInTheDocument());
    expect(api.profileClearIndex).toHaveBeenCalledWith("11111111-1111-4111-8111-111111111111");
  });

  it("lists and clears the indexes left by connects by URL", async () => {
    const { api, user } = manager([], [{ name: "127.0.0.1_6379_0", bytes: 3 * MB }]);
    expect(await screen.findByText("127.0.0.1_6379_0")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Clear index…" }));
    await user.click(within(await screen.findByRole("alertdialog")).getByRole("button", { name: "Clear index" }));
    await waitFor(() => expect(screen.queryByText("127.0.0.1_6379_0")).not.toBeInTheDocument());
    expect(api.indexOrphanClear).toHaveBeenCalledWith("127.0.0.1_6379_0");
  });

  it("says so when the list cannot be read", async () => {
    const api = fakeApi({ profilesList: () => Promise.reject("profiles.json is from a newer version"), indexesOrphans: () => Promise.resolve([]) });
    render(() => <Connections onConnect={vi.fn()} onConnectProfile={vi.fn()} error={null} onPreferences={vi.fn()} />);
    expect(await screen.findByRole("alert")).toHaveTextContent("profiles.json is from a newer version");
    expect(api.profilesList).toHaveBeenCalledTimes(1);
  });
});

describe("connect by URL", () => {
  it("passes the URL and the chosen options on, storing nothing", async () => {
    const { api, user, onConnect } = manager();
    const url = await screen.findByLabelText("Server URL");
    expect(url).toHaveValue("redis://127.0.0.1:6379/0");
    await user.clear(url);
    await user.type(url, "valkey://10.0.0.5:7000/3");
    await user.selectOptions(screen.getByRole("combobox", { name: "Environment" }), "prod");
    await user.selectOptions(screen.getByRole("combobox", { name: "Index" }), "onDemand");
    await user.selectOptions(screen.getByRole("combobox", { name: "Scan speed" }), "polite");
    await user.selectOptions(screen.getByRole("combobox", { name: "Scan from" }), "primary");
    await user.click(screen.getByRole("button", { name: "Connect" }));

    expect(onConnect).toHaveBeenCalledWith("valkey://10.0.0.5:7000/3", "prod", "polite", "primary", "onDemand");
    expect(api.profileSave).not.toHaveBeenCalled();
  });

  it("leaves the options to the environment and the size by default", async () => {
    const { user, onConnect } = manager();
    await user.click(await screen.findByRole("button", { name: "Connect" }));
    expect(onConnect).toHaveBeenCalledWith("redis://127.0.0.1:6379/0", "dev", null, "auto", null);
  });

  it("shows that a connect runs and cannot be started twice", async () => {
    const { user, onConnect } = manager();
    let finish = () => {};
    onConnect.mockImplementationOnce(() => new Promise<void>((resolve) => (finish = resolve)));
    await user.click(await screen.findByRole("button", { name: "Connect" }));
    expect(screen.getByRole("button", { name: "Connecting…" })).toBeDisabled();
    finish();
    expect(await screen.findByRole("button", { name: "Connect" })).toBeEnabled();
  });
});
