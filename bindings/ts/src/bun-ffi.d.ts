// Minimal ambient declaration for the bun:ffi surface this client uses, so
// the source typechecks under Node's tsc without depending on bun-types.
// At runtime the real module is resolved (via createRequire) only on Bun.

declare module "bun:ffi" {
  export type Pointer = number;
  export const FFIType: {
    ptr: 0;
    u64: 1;
    i32: 2;
    u32: 3;
    void: 4;
    cstring: 5;
  };
  export interface CString {
    toString(): string;
  }
  export function ptr(view: ArrayBufferView): Pointer;
  export function toArrayBuffer(ptr: Pointer, byteOffset: number, byteLength: number): ArrayBuffer;
  // The stub types symbols loosely (this module is resolved only on Bun at
  // runtime); the ffi.ts wrapper is the real type boundary.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  export function dlopen(
    path: string,
    symbols: Record<string, { args: readonly number[]; returns: number }>,
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
  ): { symbols: any; close(): void };
}
