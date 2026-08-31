import { spawn } from "node:child_process";

/**
 * Runs one command to completion and collects its output. Used for the `sem`
 * and `git` CLI calls weave_edit needs; not a general-purpose process pool.
 */

export interface RunResult {
  stdout: string;
  stderr: string;
  exitCode: number | null;
}

export function runCommand(
  command: string,
  args: string[],
  cwd: string,
  signal?: AbortSignal,
): Promise<RunResult> {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(command, args, { cwd, signal });
    let stdout = "";
    let stderr = "";

    child.stdout?.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr?.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", (err) => reject(err));
    child.on("close", (exitCode) => resolvePromise({ stdout, stderr, exitCode }));
  });
}
