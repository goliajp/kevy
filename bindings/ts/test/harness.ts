// Test harness: spawn a real kevy server for the remote-backend tests, and a
// "both backends" helper so the conformance checklist runs against embedded
// (mem://) and remote (kevy://) alike. Not a *.test.ts file, so it is not run
// as tests itself.

import net from "node:net";
import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { connect } from "../src/index.ts";

const bin = join(import.meta.dirname, "../../../target/release/kevy");

export function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.listen(0, "127.0.0.1", () => {
      const port = (srv.address() as net.AddressInfo).port;
      srv.close(() => resolve(port));
    });
    srv.on("error", reject);
  });
}

export interface Server {
  url: string;
  port: number;
  close(): void;
}

export async function spawnServer(extra: string[] = []): Promise<Server> {
  const port = await freePort();
  const dir = mkdtempSync(join(tmpdir(), "kevy-ts-srv-"));
  const proc: ChildProcess = spawn(
    bin,
    ["--bind", "127.0.0.1", "--port", String(port), "--dir", dir, ...extra],
    { stdio: "ignore" },
  );
  const url = `kevy://127.0.0.1:${port}`;
  await waitReady(url);
  return {
    url,
    port,
    close() {
      proc.kill("SIGKILL");
      try {
        rmSync(dir, { recursive: true, force: true });
      } catch {
        // best effort
      }
    },
  };
}

async function waitReady(url: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try {
      const c = await connect(url);
      await c.ping();
      c.close();
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 30));
    }
  }
  throw new Error(`server ${url} never became ready`);
}

let counter = 0;
export function uniqueMemUrl(): string {
  return `mem://test-${process.pid}-${++counter}`;
}

/** Encode a UTF-8 string to bytes (test helper). */
export const b = (s: string): Uint8Array => new TextEncoder().encode(s);
/** Decode bytes to a UTF-8 string (test helper). */
export const s = (v: Uint8Array | null): string | null => (v == null ? null : new TextDecoder().decode(v));
