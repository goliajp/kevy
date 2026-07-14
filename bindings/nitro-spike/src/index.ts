import { NitroModules } from 'react-native-nitro-modules'
import type { KevyNitro } from './specs/KevyNitro.nitro'

// Create the C++ HybridObject. The C++ constructor opens an in-memory kevy
// db; the handle lives on the native side, never crossing the bridge.
export function createKevyNitro(): KevyNitro {
  return NitroModules.createHybridObject<KevyNitro>('KevyNitro')
}

export type { KevyNitro }
