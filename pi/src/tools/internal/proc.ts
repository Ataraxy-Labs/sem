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
  /**
   * Extra variables MERGED OVER the ambient environment (never replacing
   * it -- PATH, HOME and the rest still apply). Added for sem.check({env}):
   * Django's suite needs DJANGO_SETTINGS_MODULE, matplotlib needs
   * MPLBACKEND=Agg, and countless projects need PYTHONPATH, so a verify
   * verb that cannot set one variable cannot verify those projects at all
   * (153 ImproperlyConfigured failures across 123 runs of the 2026-09-02
   * drive). A value of undefined UNSETS an inherited variable.
   */
  env?: Record<string, string | undefined>,
): Promise<RunResult> {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(command, args, { cwd, signal, ...(env ? { env: { ...process.env, ...env } } : {}) });
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
