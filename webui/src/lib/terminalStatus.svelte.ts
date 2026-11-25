/** Shared reactive terminal connection status for the top bar indicator. */
import type { Snippet } from 'svelte';
import { registerSessionReset } from './client';

let _status = $state<'connecting' | 'connected' | 'disconnected' | 'idle'>('idle');
// Shell tabs the Terminal page shows in the top bar, next to the status.
let _toolbar = $state<Snippet | null>(null);

export const terminalStatus = {
	get value() { return _status; },
	set(s: 'connecting' | 'connected' | 'disconnected' | 'idle') { _status = s; },
	get toolbar() { return _toolbar; },
	setToolbar(t: Snippet | null) { _toolbar = t; },
};

registerSessionReset(() => terminalStatus.set('idle'));
