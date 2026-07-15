// The main-process entry. Electron apps import this from their main script;
// it opens ONE kevy store (the real native engine, via the Node door) and
// hands it to bridge.js to expose over IPC. One store, shared by every
// renderer — the primary Electron design (a per-renderer wasm store is the
// documented alternative; see README "Main vs wasm renderer").
//
//   import { app, BrowserWindow, ipcMain } from "electron";
//   import { installKevyMain } from "@goliapkg/kevy-electron";
//
//   const kevy = await installKevyMain({ ipcMain, dir: "userData/kevy" });
//   // window's webPreferences: { preload: <@goliapkg/kevy-electron/preload>,
//   //                            contextIsolation: true, sandbox: true }
//   app.on("before-quit", () => kevy.dispose());

import { open, version } from "@goliapkg/kevy-node";

import { registerKevyHandlers } from "./bridge.js";

/**
 * Open a kevy store and register its IPC handlers on `ipcMain`.
 *
 * @param opts.ipcMain  Electron's ipcMain (injected, so this stays testable).
 * @param opts.dir      Persistence directory; omit for a pure in-memory store.
 * @param opts.tickMs   Pub/sub pump cadence in ms (default 50).
 * @param opts.db       Pre-opened Node-door Db to adopt instead of opening one
 *                      (the store is then left open on dispose — caller owns it).
 * @returns { db, dispose } — the store handle and a teardown that removes the
 *          handlers, stops the pump, and closes the store (unless adopted).
 */
export async function installKevyMain({ ipcMain, dir, tickMs = 50, db }) {
  const adopted = db != null;
  const store = adopted ? db : await open({ dir, tickMs });
  const bridge = registerKevyHandlers({ ipcMain, db: store, tickMs, version: await version() });
  return {
    db: store,
    dispose() {
      bridge.dispose();
      if (!adopted) store.close();
    },
  };
}
