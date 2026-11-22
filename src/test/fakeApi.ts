// A fake backend for tests. Every model reaches the backend through the
// `api` object of `src/api.ts`, so replacing its members fakes all of it,
// while the pure helpers of that module stay real.
//
// A call the test did not plan for throws and is recorded; the test then
// fails when it ends, also where a model swallows the error.

import { expect, onTestFinished, vi, type Mock } from "vitest";
import { api } from "../api";

type Api = typeof api;
type Name = keyof Api;
type EventName = { [K in Name]: K extends `on${string}` ? K : never }[Name];
type Payload<K extends EventName> = Parameters<Parameters<Api[K]>[0]>[0];

export type FakeApi = { [K in Name]: Mock<Api[K]> } & {
  /// Deliver a backend event to everything that listens for it.
  emit: <K extends EventName>(event: K, payload: Payload<K>) => void;
  /// How many listeners an event has right now.
  listeners: (event: EventName) => number;
};

/// Replace the whole `api` object for one test. `impl` holds the calls the
/// test expects; `log` is silent and events can be listened to without
/// naming them.
export function fakeApi(impl: Partial<Api> = {}): FakeApi {
  const unexpected: string[] = [];
  const handlers = new Map<string, Set<(p: unknown) => void>>();
  const target = api as unknown as Record<string, (...args: unknown[]) => unknown>;

  for (const name of Object.keys(api) as Name[]) {
    const given = impl[name] as ((...args: unknown[]) => unknown) | undefined;
    const spy = vi.spyOn(target, name);
    if (given) {
      spy.mockImplementation(given);
    } else if (name.startsWith("on")) {
      spy.mockImplementation((handler) => {
        const set = handlers.get(name) ?? new Set();
        handlers.set(name, set);
        set.add(handler as (p: unknown) => void);
        return Promise.resolve(() => void set.delete(handler as (p: unknown) => void));
      });
    } else if (name === "log") {
      spy.mockImplementation(() => Promise.resolve());
    } else {
      spy.mockImplementation(() => {
        unexpected.push(name);
        throw new Error(`unexpected api call: ${name}`);
      });
    }
  }
  onTestFinished(() => expect(unexpected, "api calls the test did not plan for").toEqual([]));

  return Object.assign(api as unknown as FakeApi, {
    emit: (event: string, payload: unknown) => handlers.get(event)?.forEach((h) => h(payload)),
    listeners: (event: string) => handlers.get(event)?.size ?? 0,
  });
}

/// Let pending promise callbacks run (a resolved fake call, an `await`).
export async function settle(rounds = 5): Promise<void> {
  for (let i = 0; i < rounds; i++) await Promise.resolve();
}
