// The example app's main process: open kevy once, expose it to the window.
//
// Run from this directory once the workspace is linked / installed:
//   npm install && npm start
//
// contextIsolation + sandbox are ON (Electron's secure defaults); the engine
// lives here in the main process and the renderer only ever sees window.kevy.

import { app, BrowserWindow, ipcMain } from "electron";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { installKevyMain } from "@goliapkg/kevy-electron";

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));

let kevy;

async function createWindow() {
  // One store, persisted under the app's userData dir. Drop `dir` for
  // pure in-memory. installKevyMain registers every window.kevy handler.
  kevy = await installKevyMain({ ipcMain, dir: join(app.getPath("userData"), "kevy") });

  const win = new BrowserWindow({
    width: 720,
    height: 560,
    webPreferences: {
      // the package's self-contained preload — safe to load under sandbox
      preload: require.resolve("@goliapkg/kevy-electron/preload"),
      contextIsolation: true,
      sandbox: true,
    },
  });
  await win.loadFile(join(here, "renderer.html"));
}

app.whenReady().then(createWindow);

app.on("activate", () => {
  if (BrowserWindow.getAllWindows().length === 0) createWindow();
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

app.on("before-quit", () => kevy?.dispose());
