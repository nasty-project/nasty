import { NastyClient } from './rpc';

/** Singleton RPC client shared across all pages */
let instance: NastyClient | null = null;
const sessionResetHandlers = new Set<() => void>();
let sessionGeneration = 0;

export function getClient(): NastyClient {
	if (!instance) {
		const wsProto = typeof window !== 'undefined' && window.location.protocol === 'https:' ? 'wss:' : 'ws:';
		const host = typeof window !== 'undefined' ? window.location.host : 'localhost';
		instance = new NastyClient(`${wsProto}//${host}/ws`);
	}
	return instance;
}

export function resetClient() {
	sessionGeneration++;
	if (instance) {
		instance.disconnect();
		instance = null;
	}
	for (const reset of sessionResetHandlers) reset();
}

export function getSessionGeneration(): number {
	return sessionGeneration;
}

/** Register state that must be cleared when the authenticated session ends. */
export function registerSessionReset(reset: () => void): () => void {
	sessionResetHandlers.add(reset);
	return () => sessionResetHandlers.delete(reset);
}
