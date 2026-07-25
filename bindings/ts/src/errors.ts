// The error taxonomy (contract §2). The TypeScript idiom is to THROW a
// KevyError subclass; each canonical variant (§2.2) is its own class so a
// caller can branch with `instanceof` or on the `.kind` tag. A structured
// store-semantic error (§2.3) is carried on StoreError.storeKind.

/** The canonical error taxonomy variant (contract §2.2). */
export type ErrorKind =
  | "store"
  | "io"
  | "protocol"
  | "readonly"
  | "invalidInput"
  | "notFound"
  | "unsupported"
  | "timedOut"
  | "closed";

/** Structured store-semantic error carried on StoreError (contract §2.3). */
export type StoreErrorKind =
  | "wrongType"
  | "notInteger"
  | "overflow"
  | "outOfRange"
  | "noSuchKey"
  | "notFloat"
  | "outOfMemory";

/**
 * The base error of the kevy client. `kind` identifies the taxonomy variant
 * so callers can branch uniformly; every variant is also its own subclass.
 */
export class KevyError extends Error {
  readonly kind: ErrorKind;
  constructor(kind: ErrorKind, message: string) {
    super(message);
    this.name = "KevyError";
    this.kind = kind;
  }
}

/** A structured store-semantic error (WRONGTYPE, non-integer, …). */
export class StoreError extends KevyError {
  readonly storeKind: StoreErrorKind;
  constructor(storeKind: StoreErrorKind) {
    super("store", `store error: ${storeKind}`);
    this.name = "StoreError";
    this.storeKind = storeKind;
  }
}

/** An OS/transport failure (socket, file, AOF). Wraps the native cause. */
export class IoError extends KevyError {
  readonly cause?: unknown;
  constructor(message: string, cause?: unknown) {
    super("io", `io error: ${message}`);
    this.name = "IoError";
    this.cause = cause;
  }
}

/** A RESP-level error: a server -ERR reply or a malformed/unexpected shape. */
export class ProtocolError extends KevyError {
  constructor(message: string) {
    super("protocol", message);
    this.name = "ProtocolError";
  }
}

/** A write rejected because the target is a read-only replica. */
export class ReadOnlyError extends KevyError {
  constructor() {
    super("readonly", "READONLY You can't write against a read only replica");
    this.name = "ReadOnlyError";
  }
}

/** A bad argument to a typed API — rejected before touching state. */
export class InvalidInputError extends KevyError {
  constructor(message: string) {
    super("invalidInput", `invalid input: ${message}`);
    this.name = "InvalidInputError";
  }
}

/** A named object (index, view, key) does not exist. */
export class NotFoundError extends KevyError {
  constructor(message: string) {
    super("notFound", `not found: ${message}`);
    this.name = "NotFoundError";
  }
}

/** The op is unavailable on this backend/build (IDX.* on embedded, TLS, …). */
export class UnsupportedError extends KevyError {
  constructor(message: string) {
    super("unsupported", `unsupported: ${message}`);
    this.name = "UnsupportedError";
  }
}

/** A bounded blocking call ran out its timeout. */
export class TimedOutError extends KevyError {
  constructor() {
    super("timedOut", "timed out");
    this.name = "TimedOutError";
  }
}

/** The connection / in-process bus is gone (EOF). */
export class ClosedError extends KevyError {
  constructor(message = "connection closed") {
    super("closed", message);
    this.name = "ClosedError";
  }
}

/**
 * Map a server error frame's text onto the structured store-error taxonomy,
 * or `null` when it is a plain protocol error (contract §2.2). Mirrors the
 * Rust/Go reference recognition so the conformance tests assert
 * Store(WrongType) uniformly on both backends.
 */
export function classifyStoreError(text: string): StoreErrorKind | null {
  if (text.startsWith("WRONGTYPE")) return "wrongType";
  if (text.startsWith("OOM")) return "outOfMemory";
  if (text.includes("is not an integer")) return "notInteger";
  if (text.includes("is not a valid float")) return "notFloat";
  if (text.includes("would overflow")) return "overflow";
  if (text.includes("no such key")) return "noSuchKey";
  if (text.includes("index out of range")) return "outOfRange";
  return null;
}
