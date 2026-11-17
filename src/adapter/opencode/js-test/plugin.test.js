// Tests for ../plugin.js. The plugin reads its hook table at import, so each test mocks the file
// read, resets the module registry, and imports the real plugin again.
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/** Import the plugin with `table` as the content of llmenv-hooks.json (a string, or an Error). */
async function loadPlugin(table) {
  vi.resetModules();
  vi.doMock("node:fs", () => ({
    readFileSync: () => {
      if (table instanceof Error) throw table;
      return typeof table === "string" ? table : JSON.stringify(table);
    },
  }));
  return import("../plugin.js");
}

const hook = (event, command, extra = {}) => ({ event, command, ...extra });
const tableOf = (...hooks) => ({ hooks, tools: {}, mcpServers: {} });

/** A fake opencode context whose event stream yields `events` and then waits. */
function fakeContext(events) {
  const handlers = {};
  const register = (kind) => async (name, fn) => {
    handlers[`${kind}.${name}`] = fn;
  };
  const ctx = {
    location: { directory: "/work" },
    tool: { hook: register("tool") },
    session: { hook: register("session") },
    event: {
      subscribe: async function* () {
        yield* events;
        await new Promise(() => {});
      },
    },
  };
  return { ctx, handlers };
}

let logged;

beforeEach(() => {
  logged = vi.spyOn(console, "error").mockImplementation(() => {});
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.doUnmock("node:fs");
});

describe("session end", () => {
  const created = { type: "session.created", data: { sessionID: "s1" } };
  const deleted = { type: "session.deleted", data: { sessionID: "s1" } };

  it("logs the text of a SessionEnd hook, because no request can carry it", async () => {
    const { default: plugin } = await loadPlugin(
      tableOf(hook("SessionEnd", `printf '%s' '{"hookSpecificOutput":{"additionalContext":"bye text"}}'`)),
    );
    const { ctx } = fakeContext([created, deleted]);
    await plugin.setup(ctx);
    await vi.waitFor(() => expect(logged).toHaveBeenCalledWith("bye text"));
  });

  it("logs the block text of a SessionEnd hook", async () => {
    const { default: plugin } = await loadPlugin(
      tableOf(hook("SessionEnd", `echo 'end blocked' >&2; exit 2`)),
    );
    const { ctx } = fakeContext([created, deleted]);
    await plugin.setup(ctx);
    await vi.waitFor(() => expect(logged).toHaveBeenCalledWith("end blocked"));
  });

  it("ends a session once, even when the event comes twice", async () => {
    const { default: plugin } = await loadPlugin(
      tableOf(hook("SessionEnd", `printf 'once text'`)),
    );
    const { ctx } = fakeContext([created, deleted, deleted]);
    await plugin.setup(ctx);
    await vi.waitFor(() => expect(logged).toHaveBeenCalledWith("once text"));
    const times = logged.mock.calls.filter(([text]) => text === "once text").length;
    expect(times).toBe(1);
  });

  it("ends every open session when the plugin shuts down", async () => {
    const { default: plugin } = await loadPlugin(
      tableOf(hook("SessionStart", `printf 'start text'`), hook("SessionEnd", `printf 'shutdown text'`)),
    );
    const { ctx, handlers } = fakeContext([created]);
    const shutdown = await plugin.setup(ctx);
    // The start text reaches the next request once the session exists.
    await vi.waitFor(async () => {
      const e = { sessionID: "s1", system: [] };
      await handlers["session.context"](e);
      expect(e.system.map((part) => part.text)).toContain("start text");
    });
    expect(logged).not.toHaveBeenCalledWith("shutdown text");
    await shutdown();
    expect(logged).toHaveBeenCalledWith("shutdown text");
  });
});

describe("hook table", () => {
  it("refuses a tool call when the table cannot be read", async () => {
    const { default: plugin } = await loadPlugin(new Error("no such file"));
    const { ctx, handlers } = fakeContext([]);
    await plugin.setup(ctx);
    await expect(handlers["tool.execute.before"]({ sessionID: "s1", tool: "bash", input: {} })).rejects.toThrow(
      /cannot be read: no such file.*llmenv materialize/s,
    );
  });

  it("refuses a tool call when the table has the wrong shape", async () => {
    const { default: plugin } = await loadPlugin({ hooks: "nope", tools: {} });
    const { ctx, handlers } = fakeContext([]);
    await plugin.setup(ctx);
    await expect(handlers["tool.execute.before"]({ sessionID: "s1", tool: "bash", input: {} })).rejects.toThrow(
      /does not have the hook table shape/,
    );
  });
});

describe("tool names", () => {
  it("maps an opencode tool to its Claude name, and back", async () => {
    const plugin = await loadPlugin({ hooks: [], tools: { shell: { name: "Bash", args: { cmd: "command" } } } });
    const neutral = plugin.toNeutral("shell", { cmd: "ls" });
    expect(neutral.name).toBe("Bash");
    expect(neutral.input).toEqual({ command: "ls" });
    expect(plugin.fromNeutral("shell", neutral.input)).toEqual({ cmd: "ls" });
  });
});
