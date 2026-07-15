const { getDefaultConfig, mergeConfig } = require('@react-native/metro-config');
const path = require('path');
const fs = require('fs');

/**
 * Metro configuration
 * https://reactnative.dev/docs/metro
 *
 * expo-kevy is consumed here as a `file:..` dependency, which npm links as a
 * symlink into node_modules pointing at bindings/expo — OUTSIDE this app's
 * project root. Metro only watches the project root by default, so it must
 * be told to watch the symlink target too, and to ignore the sibling
 * example apps under bindings/expo that carry their own react-native copies
 * (those would otherwise collide as duplicate Haste modules).
 *
 * @type {import('@react-native/metro-config').MetroConfig}
 */
const projectRoot = __dirname;
const expoKevyRoot = fs.realpathSync(
  path.resolve(projectRoot, 'node_modules', 'expo-kevy'),
);

const config = {
  watchFolders: [expoKevyRoot],
  resolver: {
    // The linked package ships no node_modules of its own; resolve its peer
    // deps (react, react-native, expo-modules-core) from this app.
    nodeModulesPaths: [path.resolve(projectRoot, 'node_modules')],
    // The sibling Expo example app under bindings/expo ships its own
    // react-native copy — crawling it would collide as duplicate Haste
    // modules. This app's own node_modules dedupe by realpath, so they
    // need no exclusion. blockList takes an array of RegExp directly.
    blockList: [new RegExp(`${path.join(expoKevyRoot, 'example')}/.*`)],
  },
};

module.exports = mergeConfig(getDefaultConfig(projectRoot), config);
