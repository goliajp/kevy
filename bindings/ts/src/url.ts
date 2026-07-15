// Connect-URL routing (contract §1.1). A single connect(url) chooses the
// backend from the scheme; TLS/AUTH and unknown schemes are rejected before
// any I/O. The same business code runs against embedded or remote by
// changing only the URL string.

import { InvalidInputError, UnsupportedError } from "./errors.ts";

/** The backend an input URL resolves to. */
export type TargetKind = "memAnon" | "memNamed" | "file" | "remote";

/** A parsed connect URL. */
export interface Target {
  readonly kind: TargetKind;
  readonly name: string; // mem://<name>
  readonly path: string; // file://<path>
  readonly host: string; // remote host
  readonly port: number; // remote port (default 6379)
  readonly db: number | null; // remote /db index (null for tcp://)
  readonly url: string; // original URL
}

/** The process-global registry key for shared embedded targets, or "". */
export function registryKey(t: Target): string {
  if (t.kind === "memNamed") return "mem://" + t.name;
  if (t.kind === "file") return "file://" + t.path;
  return "";
}

/** Resolve a connect URL to a target, rejecting TLS/AUTH/unknown up front. */
export function parseConnectURL(url: string): Target {
  const sep = url.indexOf("://");
  if (sep < 0) throw new InvalidInputError("URL missing '://'");
  const scheme = url.slice(0, sep);
  const rest = url.slice(sep + 3);
  switch (scheme) {
    case "mem":
      return rest === ""
        ? base({ kind: "memAnon", url })
        : base({ kind: "memNamed", name: rest, url });
    case "file":
      if (rest === "") {
        throw new InvalidInputError(
          "file:// URL must include a path (e.g. file:///var/lib/myapp)",
        );
      }
      return base({ kind: "file", path: rest, url });
    case "kevy":
    case "redis":
    case "tcp":
      return parseRemote(scheme, rest, url);
    case "rediss":
    case "kevys":
      throw new UnsupportedError(
        "TLS schemes (rediss://, kevys://) are unsupported — kevy has no TLS",
      );
    default:
      throw new InvalidInputError(`unknown URL scheme '${scheme}://'`);
  }
}

function parseRemote(scheme: string, rest: string, url: string): Target {
  if (rest.includes("@")) {
    throw new UnsupportedError(
      "userinfo (user:pass@host) is unsupported — kevy has no AUTH",
    );
  }
  let authority = rest;
  let path = "";
  const slash = rest.indexOf("/");
  if (slash >= 0) {
    authority = rest.slice(0, slash);
    path = rest.slice(slash + 1);
  }
  const [host, port] = parseAuthority(authority);
  let db: number | null = null;
  // tcp:// is raw: it never carries a db (§1.1). kevy:// / redis:// honour /N.
  if (scheme !== "tcp" && path !== "") {
    if (!/^\d+$/.test(path)) {
      throw new InvalidInputError(
        `bad db index: '${path}' (expected a non-negative integer)`,
      );
    }
    db = Number(path);
  }
  return base({ kind: "remote", host, port, db, url });
}

function parseAuthority(authority: string): [string, number] {
  let host = authority;
  let port = 6379;
  const i = authority.lastIndexOf(":");
  if (i >= 0) {
    host = authority.slice(0, i);
    const p = authority.slice(i + 1);
    if (!/^\d+$/.test(p) || Number(p) > 65535) {
      throw new InvalidInputError("bad port: " + p);
    }
    port = Number(p);
  }
  if (host === "") throw new InvalidInputError("empty host");
  return [host, port];
}

function base(over: Partial<Target> & { kind: TargetKind; url: string }): Target {
  return { name: "", path: "", host: "", port: 6379, db: null, ...over };
}
