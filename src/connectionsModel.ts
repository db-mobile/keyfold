// State and actions of the connection manager (CONCEPT 3.4). The markup
// is in `Connections.tsx`. Passwords go to the OS keychain through the
// backend; the webview only ever sends one, it never reads one back.

import { createSignal, onMount } from "solid-js";
import { api, fmtBytes, type IndexMode, type OrphanIndex, type ProfileInput, type ScanProfile, type ScanTarget, type SshSettings, type StoredProfile } from "./api";
import { ask } from "./dialogModel";

export type Draft = ProfileInput & { passwordSaved: boolean; sshSecretSaved: boolean };
export type SavedProfile = Omit<StoredProfile, "indexBytes">;

export interface ConnectionsProps {
  onConnect: (url: string, tag: string, profile: ScanProfile | null, target: ScanTarget, indexMode: IndexMode | null) => Promise<void>;
  onConnectProfile: (id: string) => Promise<void>;
  error: string | null;
  onPreferences: () => void;
}

export interface EditorProps {
  draft: Draft;
  onClose: () => void;
  onSaved: (p: SavedProfile, connect: boolean) => Promise<void>;
}

export interface QuickConnectProps {
  onConnect: ConnectionsProps["onConnect"];
}

const blank = (): Draft => ({
  id: null,
  name: "",
  host: "127.0.0.1",
  port: 6379,
  db: 0,
  username: null,
  tls: false,
  tlsInsecure: false,
  tag: "dev",
  scanProfile: null,
  scanTarget: "auto",
  ephemeralIndex: null,
  indexMode: null,
  ssh: null,
  passwordSaved: false,
  sshSecretSaved: false,
});

const blankSsh = (): SshSettings => ({ host: "", port: 22, user: "", auth: "agent", keyPath: null });

export function createConnectionsModel(props: ConnectionsProps) {
  const [profiles, setProfiles] = createSignal<StoredProfile[]>([]);
  const [listError, setListError] = createSignal<string | null>(null);
  const [editing, setEditing] = createSignal<Draft | null>(null);
  const [busy, setBusy] = createSignal<string | null>(null);

  const [orphans, setOrphans] = createSignal<OrphanIndex[]>([]);

  const reload = () =>
    Promise.all([api.profilesList(), api.indexesOrphans()])
      .then(([p, o]) => {
        setProfiles(p);
        setOrphans(o);
        void api.log("info", `connections: ${p.length} stored, ${o.length} other indexes (${fmtBytes(o.reduce((n, x) => n + x.bytes, 0))})`);
        setListError(null);
      })
      .catch((e) => setListError(String(e)));
  onMount(reload);

  /// Run an action on the list, then read the list again. Why the action
  /// failed stays on screen: the reload would otherwise clear it.
  const change = async (action: Promise<void>) => {
    const failed = await action.then(
      () => null,
      (e) => String(e),
    );
    await reload();
    if (failed !== null) setListError(failed);
  };

  const connect = async (p: Pick<StoredProfile, "id">) => {
    if (busy()) return;
    setBusy(p.id);
    try {
      await props.onConnectProfile(p.id);
    } finally {
      setBusy(null);
    }
  };

  const remove = async (p: StoredProfile) => {
    const ok = await ask({
      title: `Delete ${p.name}?`,
      body: `Removes the stored connection${p.passwordSaved ? ", its password in the OS keychain" : ""} and its index${
        p.indexBytes ? ` (${fmtBytes(p.indexBytes)})` : ""
      }. Nothing on the server changes.`,
      confirm: "Delete",
      danger: true,
    });
    if (!ok) return;
    await change(api.profileDelete(p.id));
  };

  const clearIndex = async (p: StoredProfile) => {
    const ok = await ask({
      title: `Clear the index of ${p.name}?`,
      body: `Deletes ${fmtBytes(p.indexBytes)} on disk. The next connect scans the keyspace again.`,
      confirm: "Clear index",
    });
    if (!ok) return;
    await change(api.profileClearIndex(p.id));
  };

  const clearOrphan = async (o: OrphanIndex) => {
    const ok = await ask({
      title: `Clear the index ${o.name}?`,
      body: `Deletes ${fmtBytes(o.bytes)} on disk. A later connect to that server scans the keyspace again.`,
      confirm: "Clear index",
    });
    if (!ok) return;
    await change(api.indexOrphanClear(o.name));
  };

  return {
    profiles,
    orphans,
    clearOrphan: (o: OrphanIndex) => void clearOrphan(o),
    orphanSize: (o: OrphanIndex) => fmtBytes(o.bytes),
    listError,
    editing,
    /// Nothing else may start while a connect runs.
    locked: () => busy() !== null,
    connecting: (p: StoredProfile) => busy() === p.id,
    connect: (p: StoredProfile) => void connect(p),
    remove: (p: StoredProfile) => void remove(p),
    clearIndex: (p: StoredProfile) => void clearIndex(p),
    create: () => setEditing(blank()),
    edit: (p: StoredProfile) => setEditing({ ...p }),
    closeEditor: () => setEditing(null),
    saved: async (saved: SavedProfile, thenConnect: boolean) => {
      setEditing(null);
      await reload();
      if (thenConnect) await connect(saved);
    },
    where: (p: StoredProfile) =>
      `${p.username ? `${p.username}@` : ""}${p.host}:${p.port}/${p.db}${p.tls ? ", TLS" : ""}${p.ssh ? `, via ${p.ssh.user}@${p.ssh.host}` : ""}`,
    indexLabel: (p: StoredProfile) => (p.indexBytes ? `Index ${fmtBytes(p.indexBytes)}` : "No index"),
  };
}

export function createEditorModel(props: EditorProps) {
  const [d, setD] = createSignal<Draft>({ ...props.draft });
  const [password, setPassword] = createSignal("");
  const [forget, setForget] = createSignal(false);
  const [sshSecret, setSshSecret] = createSignal("");
  const [forgetSsh, setForgetSsh] = createSignal(false);
  /// What the tunnel fields held before it was switched off, to bring back.
  let sshDraft: SshSettings = props.draft.ssh ?? blankSsh();
  const setSsh = (patch: Partial<SshSettings>) => {
    sshDraft = { ...(d().ssh ?? sshDraft), ...patch };
    set({ ssh: sshDraft });
  };
  const [url, setUrl] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal(false);
  const set = (patch: Partial<Draft>) => setD({ ...d(), ...patch });

  const fill = async () => {
    setError(null);
    try {
      const u = await api.profileFromUrl(url());
      set({ host: u.host, port: u.port, db: u.db, username: u.username, tls: u.tls, tlsInsecure: u.tlsInsecure, name: d().name || u.name });
      if (u.password) {
        setPassword(u.password);
        setForget(false);
      }
      setUrl("");
    } catch (e) {
      setError(String(e));
    }
  };

  const save = async (thenConnect: boolean) => {
    setError(null);
    setBusy(true);
    try {
      const { passwordSaved: _saved, sshSecretSaved: _sshSaved, ...input } = d();
      // null keeps what the keychain has, "" removes it
      const pw = password() !== "" ? password() : forget() ? "" : null;
      const ssh = sshSecret() !== "" ? sshSecret() : forgetSsh() ? "" : null;
      const saved = await api.profileSave(input, pw, ssh);
      await props.onSaved(saved, thenConnect);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return {
    d,
    password,
    forget,
    url,
    error,
    busy,
    title: () => (d().id ? `Edit ${props.draft.name}` : "New connection"),
    canFill: () => url().trim() !== "",
    /// The stored password can only be removed while no new one is typed.
    canForget: () => d().passwordSaved && password() === "",
    passwordHint: () => (d().passwordSaved ? "Unchanged, in the OS keychain" : "None"),
    tlsInsecure: () => d().tls && d().tlsInsecure,
    ephemeral: () => (d().ephemeralIndex === null ? "" : d().ephemeralIndex ? "on" : "off"),
    scanProfile: () => d().scanProfile ?? "",
    indexMode: () => d().indexMode ?? "",

    // the SSH tunnel: its fields stay in place while it is off, so the dialog keeps its size
    sshOn: () => d().ssh !== null,
    sshOff: () => d().ssh === null,
    ssh: () => d().ssh ?? sshDraft,
    sshSecret,
    forgetSsh,
    setSshOn: (on: boolean) => set({ ssh: on ? sshDraft : null }),
    setSshHost: (host: string) => setSsh({ host }),
    setSshPort: (port: string) => setSsh({ port: Number(port) }),
    setSshUser: (user: string) => setSsh({ user }),
    setSshAuth: (auth: string) => setSsh({ auth: auth as SshSettings["auth"] }),
    setSshKeyPath: (keyPath: string) => setSsh({ keyPath: keyPath || null }),
    setSshSecret,
    setForgetSsh,
    sshUsesKey: () => (d().ssh ?? sshDraft).auth === "key",
    sshNeedsSecret: () => d().ssh !== null && (d().ssh ?? sshDraft).auth !== "agent",
    sshSecretLabel: () => ((d().ssh ?? sshDraft).auth === "key" ? "Key passphrase" : "SSH password"),
    sshSecretHint: () => (d().sshSecretSaved ? "Unchanged, in the OS keychain" : (d().ssh ?? sshDraft).auth === "key" ? "None" : "Required"),
    canForgetSsh: () => d().sshSecretSaved && sshSecret() === "" && d().ssh !== null && (d().ssh ?? sshDraft).auth !== "agent",
    setIndexMode: (value: string) => set({ indexMode: (value || null) as IndexMode | null }),
    setUrl,
    setPassword,
    setForget,
    setName: (name: string) => set({ name }),
    setHost: (host: string) => set({ host }),
    setPort: (port: string) => set({ port: Number(port) }),
    setDb: (db: string) => set({ db: Number(db) }),
    setUsername: (username: string) => set({ username: username || null }),
    setTls: (tls: boolean) => set({ tls, tlsInsecure: tls && d().tlsInsecure }),
    setTlsInsecure: (tlsInsecure: boolean) => set({ tlsInsecure }),
    setTag: (tag: string) => set({ tag: tag as Draft["tag"] }),
    setEphemeral: (value: string) => set({ ephemeralIndex: value === "" ? null : value === "on" }),
    setScanProfile: (value: string) => set({ scanProfile: (value || null) as ScanProfile | null }),
    setScanTarget: (value: string) => set({ scanTarget: value as ScanTarget }),
    fill: () => void fill(),
    save: () => void save(false),
    saveAndConnect: () => void save(true),
    focusLater: (el: HTMLElement) => setTimeout(() => el.focus()),
  };
}

export function createQuickConnectModel(props: QuickConnectProps) {
  const [url, setUrl] = createSignal("redis://127.0.0.1:6379/0");
  const [tag, setTag] = createSignal("dev");
  const [profile, setProfile] = createSignal<ScanProfile | "">("");
  const [target, setTarget] = createSignal<ScanTarget>("auto");
  const [indexMode, setIndexMode] = createSignal<IndexMode | "">("");
  const [busy, setBusy] = createSignal(false);

  const submit = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    try {
      await props.onConnect(url(), tag(), profile() || null, target(), indexMode() || null);
    } finally {
      setBusy(false);
    }
  };

  return {
    url,
    tag,
    profile,
    target,
    indexMode,
    busy,
    setUrl,
    setTag,
    setIndexMode: (value: string) => setIndexMode(value as IndexMode | ""),
    setProfile: (value: string) => setProfile(value as ScanProfile | ""),
    setTarget: (value: string) => setTarget(value as ScanTarget),
    submit,
  };
}
