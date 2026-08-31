import { type ChildProcessWithoutNullStreams, spawn } from "node:child_process";

/**
 * Minimal MCP stdio client: JSON-RPC 2.0 framed as one JSON object per
 * newline (protocol 2024-11-05). Spawns the server, performs the
 * initialize handshake, and exposes tools/list + tools/call.
 *
 * Restarts the child process once if it crashes unexpectedly; a second
 * crash leaves the client dead until `start()` is called again explicitly.
 */

export interface McpToolInfo {
  name: string;
  description?: string;
  inputSchema: Record<string, unknown>;
}

export interface McpTextContent {
  type: "text";
  text: string;
}

export interface McpCallResult {
  content: McpTextContent[];
  isError?: boolean;
}

export interface McpClientOptions {
  id: string;
  command: string;
  args?: string[];
  env?: Record<string, string>;
  cwd?: string;
  /** Per-call timeout in milliseconds. Defaults to 30_000. */
  requestTimeoutMs?: number;
  /** Called with each stderr chunk the server writes. */
  onStderr?: (chunk: string) => void;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (reason: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

interface JsonRpcResponse {
  jsonrpc: "2.0";
  id: number | string;
  result?: unknown;
  error?: { code: number; message: string; data?: unknown };
}

const PROTOCOL_VERSION = "2024-11-05";
const DEFAULT_TIMEOUT_MS = 30_000;

function isJsonRpcResponse(value: unknown): value is JsonRpcResponse {
  if (typeof value !== "object" || value === null) return false;
  const record = value as Record<string, unknown>;
  return record.jsonrpc === "2.0" && "id" in record;
}

export class McpClientCrashedError extends Error {
  constructor(id: string) {
    super(`MCP server "${id}" crashed and the one allowed restart was already used.`);
    this.name = "McpClientCrashedError";
  }
}

export class McpClient {
  readonly id: string;
  private readonly options: McpClientOptions;
  private child: ChildProcessWithoutNullStreams | undefined;
  private stdoutBuffer = "";
  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private restarted = false;
  private dead = false;
  private startPromise: Promise<void> | undefined;

  constructor(options: McpClientOptions) {
    this.id = options.id;
    this.options = options;
  }

  get isRunning(): boolean {
    return this.child !== undefined && !this.dead;
  }

  /** Spawn the server and complete the initialize handshake. Idempotent while starting. */
  async start(): Promise<void> {
    if (this.startPromise) return this.startPromise;
    this.startPromise = this.spawnAndInitialize();
    try {
      await this.startPromise;
    } finally {
      this.startPromise = undefined;
    }
  }

  private async spawnAndInitialize(): Promise<void> {
    this.dead = false;
    const child = spawn(this.options.command, this.options.args ?? [], {
      cwd: this.options.cwd,
      env: { ...process.env, ...this.options.env },
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child = child;
    this.stdoutBuffer = "";

    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => this.onStdoutData(chunk));

    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => this.options.onStderr?.(chunk));

    child.on("exit", () => this.onExit());
    child.on("error", (err) => this.failAllPending(err instanceof Error ? err : new Error(String(err))));

    await this.request("initialize", {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "pi-sem", version: "0.1.0" },
    });
    this.notify("notifications/initialized");
  }

  private onExit(): void {
    const wasIntentional = this.child === undefined;
    this.child = undefined;
    if (wasIntentional) return;

    this.failAllPending(new Error(`MCP server "${this.id}" exited unexpectedly.`));

    if (this.restarted) {
      this.dead = true;
      return;
    }
    this.restarted = true;
    // Fire-and-forget: the next call to start()/listTools()/callTool() awaits
    // startPromise if still in flight, otherwise callers see a dead client
    // until they call start() again. We proactively attempt one respawn here.
    void this.spawnAndInitialize().catch(() => {
      this.dead = true;
    });
  }

  private failAllPending(err: Error): void {
    for (const [, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(err);
    }
    this.pending.clear();
  }

  private onStdoutData(chunk: string): void {
    this.stdoutBuffer += chunk;
    let newlineIndex = this.stdoutBuffer.indexOf("\n");
    while (newlineIndex !== -1) {
      const line = this.stdoutBuffer.slice(0, newlineIndex);
      this.stdoutBuffer = this.stdoutBuffer.slice(newlineIndex + 1);
      this.handleLine(line);
      newlineIndex = this.stdoutBuffer.indexOf("\n");
    }
  }

  private handleLine(line: string): void {
    const trimmed = line.trim();
    if (!trimmed) return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(trimmed);
    } catch {
      return; // Not a JSON-RPC line; ignore (servers may log to stdout by mistake).
    }
    if (!isJsonRpcResponse(parsed)) return;
    const id = typeof parsed.id === "number" ? parsed.id : Number(parsed.id);
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);
    clearTimeout(pending.timer);
    if (parsed.error) {
      pending.reject(new Error(`${this.id}: ${parsed.error.message} (code ${parsed.error.code})`));
    } else {
      pending.resolve(parsed.result);
    }
  }

  private notify(method: string, params?: unknown): void {
    this.write({ jsonrpc: "2.0", method, params });
  }

  private write(message: Record<string, unknown>): void {
    if (!this.child) throw new Error(`MCP server "${this.id}" is not running.`);
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  private request(method: string, params?: unknown, signal?: AbortSignal): Promise<unknown> {
    const id = this.nextId++;
    const timeoutMs = this.options.requestTimeoutMs ?? DEFAULT_TIMEOUT_MS;

    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`${this.id}: request "${method}" timed out after ${timeoutMs}ms.`));
      }, timeoutMs);

      const onAbort = () => {
        this.pending.delete(id);
        clearTimeout(timer);
        this.notify("notifications/cancelled", { requestId: id, reason: "aborted by caller" });
        reject(new Error(`${this.id}: request "${method}" was aborted.`));
      };
      if (signal) {
        if (signal.aborted) {
          clearTimeout(timer);
          reject(new Error(`${this.id}: request "${method}" was aborted.`));
          return;
        }
        signal.addEventListener("abort", onAbort, { once: true });
      }

      this.pending.set(id, {
        resolve: (value) => {
          signal?.removeEventListener("abort", onAbort);
          resolve(value);
        },
        reject: (err) => {
          signal?.removeEventListener("abort", onAbort);
          reject(err);
        },
        timer,
      });

      try {
        this.write({ jsonrpc: "2.0", id, method, params });
      } catch (err) {
        this.pending.delete(id);
        clearTimeout(timer);
        reject(err instanceof Error ? err : new Error(String(err)));
      }
    });
  }

  async listTools(signal?: AbortSignal): Promise<McpToolInfo[]> {
    if (this.dead) throw new McpClientCrashedError(this.id);
    const result = (await this.request("tools/list", {}, signal)) as { tools: McpToolInfo[] };
    return result.tools;
  }

  async callTool(name: string, args: unknown, signal?: AbortSignal): Promise<McpCallResult> {
    if (this.dead) throw new McpClientCrashedError(this.id);
    const result = (await this.request("tools/call", { name, arguments: args }, signal)) as McpCallResult;
    return result;
  }

  /** Clean shutdown: closes stdin and gives the server a moment to exit before killing it. */
  async stop(): Promise<void> {
    const child = this.child;
    if (!child) return;
    this.child = undefined; // marks the coming exit as intentional
    this.failAllPending(new Error(`MCP server "${this.id}" was stopped.`));

    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        child.kill("SIGKILL");
        resolve();
      }, 2000);
      child.once("exit", () => {
        clearTimeout(timer);
        resolve();
      });
      child.stdin.end();
    });
  }
}
