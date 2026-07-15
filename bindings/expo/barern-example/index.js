/**
 * @format
 */

// Install Expo's runtime polyfills (the "winter" runtime: TextDecoder, URL,
// structuredClone, …). Managed Expo apps get this implicitly via their
// entry point; a bare RN app must import it. expo-kevy decodes RESP bytes
// with TextDecoder, which Hermes does not provide on its own.
import 'expo';

import { AppRegistry } from 'react-native';
import App from './App';
import { name as appName } from './app.json';

AppRegistry.registerComponent(appName, () => App);
