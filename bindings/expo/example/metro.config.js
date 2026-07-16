// Metro config for the expo-kevy example. The kevy doors are consumed as
// local symlinked packages (expo-kevy = file:.., react-native-kevy-nitro =
// file:../../nitro), so Metro must (a) watch those out-of-root folders and
// (b) resolve their peer deps (react-native-nitro-modules, etc.) from THIS
// app's node_modules — a symlinked package has no node_modules of its own.
const { getDefaultConfig } = require("expo/metro-config");
const path = require("path");

const projectRoot = __dirname;
const repoRoot = path.resolve(projectRoot, "../../..");

const config = getDefaultConfig(projectRoot);

// Serve files from the symlinked doors (outside the app root).
config.watchFolders = [
  path.resolve(projectRoot, ".."), // bindings/expo (expo-kevy)
  path.resolve(projectRoot, "../../nitro"), // react-native-kevy-nitro
  repoRoot,
];

// Peer deps of the symlinked doors resolve from the app's node_modules.
config.resolver.nodeModulesPaths = [
  path.resolve(projectRoot, "node_modules"),
];
config.resolver.unstable_enableSymlinks = true;

module.exports = config;
