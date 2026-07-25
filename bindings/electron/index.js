// @goliapkg/kevy-electron — kevy embedded in an Electron app.
//
// The main process owns one real native kevy store; renderers reach it over
// a contextBridge preload + IPC. This is the main-process entry; the renderer
// side is the sibling preload (import "@goliapkg/kevy-electron/preload" as a
// window's webPreferences.preload). See README.md.

export { installKevyMain } from "./main.js";
export { registerKevyHandlers } from "./bridge.js";
export { CHANNELS, KevyError } from "./wire.js";
