// Browser-globals stubs for vitest's node environment.
//
// rpc.ts has `static debug = ... localStorage.getItem(...)` which evaluates
// at module-import time. Vitest's node env provides a partial localStorage
// without getItem; without a stub the import throws before any test runs.
globalThis.localStorage = {
	getItem: () => null,
	setItem: () => {},
	removeItem: () => {},
	clear: () => {},
	key: () => null,
	length: 0
};
