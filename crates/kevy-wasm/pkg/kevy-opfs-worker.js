// @goliajp/kevy — OPFS storage worker. Owns a FileSystemSyncAccessHandle
// on `kevy-wasm/<name>.aof` (sync handles are worker-only) and serves the
// loader's pump: load the whole log at startup, append write frames,
// replace the file with a compacted image. Every append/replace flushes
// before acknowledging, so a resolved request is durable.

let handle = null;
let size = 0;

async function handleRequest(op, name, buf) {
  switch (op) {
    // Open returns the current file content in the same round trip, so
    // startup replay costs one worker exchange instead of two.
    case "open": {
      const root = await navigator.storage.getDirectory();
      const dir = await root.getDirectoryHandle("kevy-wasm", { create: true });
      const file = await dir.getFileHandle(`${name}.aof`, { create: true });
      handle = await file.createSyncAccessHandle();
      size = handle.getSize();
      const data = new Uint8Array(size);
      if (size > 0) handle.read(data, { at: 0 });
      return data;
    }
    case "append": {
      handle.write(buf, { at: size });
      size += buf.byteLength;
      handle.flush();
      return undefined;
    }
    case "replace": {
      handle.truncate(0);
      if (buf.byteLength > 0) handle.write(buf, { at: 0 });
      size = buf.byteLength;
      handle.flush();
      return undefined;
    }
    case "close": {
      if (handle) handle.close();
      handle = null;
      size = 0;
      return undefined;
    }
    default:
      throw new Error(`unknown op: ${op}`);
  }
}

onmessage = async (ev) => {
  const { id, op, name, buf } = ev.data;
  try {
    const out = await handleRequest(op, name, buf);
    if (out) postMessage({ id, ok: true, buf: out }, [out.buffer]);
    else postMessage({ id, ok: true });
  } catch (err) {
    postMessage({ id, ok: false, err: String((err && err.message) || err) });
  }
};
