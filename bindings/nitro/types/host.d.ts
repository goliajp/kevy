// Host globals this door relies on, declared rather than pulled in with
// `lib: ["dom"]`.
//
// The React Native runtime provides `queueMicrotask`, but it is not part
// of the ES library and this package's tsconfig deliberately does not
// claim the DOM — a door that typechecks against `window` and `document`
// would be checking against a host it never runs on. Naming the one
// global it actually needs states the contract instead.
declare function queueMicrotask(callback: () => void): void;
