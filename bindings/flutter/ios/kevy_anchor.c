// Link anchor for the dart:ffi door. flutter_kevy calls the engine
// through DynamicLibrary.process(), which resolves symbols at runtime —
// so at link time nothing references libkevy_ffi.a and the linker
// dead-strips it (Xcode: "Library 'kevy_ffi' not found"). This one
// forced reference to a single engine symbol pulls the static library's
// object in, keeping every kevy_* export in the app binary where
// DynamicLibrary.process() can find them. (The Swift/expo door needs no
// anchor: its Swift shell calls the symbols directly.)
#include <stdint.h>

extern uint32_t kevy_abi(void);

__attribute__((used, visibility("default"))) uint32_t
_kevy_flutter_link_anchor(void) {
    return kevy_abi();
}
