<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { Terminal } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';
	import { WebLinksAddon } from '@xterm/addon-web-links';
	import { getToken } from '$lib/auth';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';

	import type { IDisposable } from '@xterm/xterm';

	let terminalEl: HTMLDivElement;
	let term: Terminal | null = null;
	let fitAddon: FitAddon | null = null;
	let ws: WebSocket | null = null;
	let status = $state<'connecting' | 'connected' | 'disconnected'>('connecting');
	let termListeners: IDisposable[] = [];

	onMount(() => {
		term = new Terminal({
			cursorBlink: true,
			fontSize: 14,
			fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
			theme: {
				background: '#0f1117',
				foreground: '#e0e0e0',
				cursor: '#e0e0e0',
				selectionBackground: '#2563eb55',
				black: '#0f1117',
				red: '#dc2626',
				green: '#4ade80',
				yellow: '#f59e0b',
				blue: '#2563eb',
				magenta: '#a855f7',
				cyan: '#22d3ee',
				white: '#e0e0e0',
				brightBlack: '#4b5563',
				brightRed: '#f87171',
				brightGreen: '#86efac',
				brightYellow: '#fcd34d',
				brightBlue: '#60a5fa',
				brightMagenta: '#c084fc',
				brightCyan: '#67e8f9',
				brightWhite: '#ffffff',
			},
		});

		fitAddon = new FitAddon();
		term.loadAddon(fitAddon);
		term.loadAddon(new WebLinksAddon());

		term.open(terminalEl);
		fitAddon.fit();

		connect();

		const resizeObserver = new ResizeObserver(() => {
			fitAddon?.fit();
		});
		resizeObserver.observe(terminalEl);

		return () => {
			resizeObserver.disconnect();
		};
	});

	onDestroy(() => {
		ws?.close();
		term?.dispose();
	});

	function connect() {
		const token = getToken();
		if (!token) {
			status = 'disconnected';
			term?.writeln('\r\n\x1b[31mNot authenticated. Please sign in first.\x1b[0m');
			return;
		}

		const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
		ws = new WebSocket(`${proto}//${window.location.host}/ws/terminal`);
		status = 'connecting';

		ws.onopen = () => {
			const cols = term?.cols ?? 80;
			const rows = term?.rows ?? 24;
			ws?.send(JSON.stringify({ token, cols, rows }));
		};

		ws.onmessage = (event) => {
			const data = event.data;

			if (status === 'connecting') {
				try {
					const msg = JSON.parse(data);
					if (msg.authenticated) {
						status = 'connected';

						termListeners.forEach(l => l.dispose());
						termListeners = [];

						termListeners.push(term!.onData((input) => {
							if (ws?.readyState === WebSocket.OPEN) {
								ws.send(input);
							}
						}));

						termListeners.push(term!.onResize(({ cols, rows }) => {
							if (ws?.readyState === WebSocket.OPEN) {
								ws.send(JSON.stringify({ type: 'resize', cols, rows }));
							}
						}));

						return;
					}
					if (msg.error) {
						status = 'disconnected';
						term?.writeln(`\r\n\x1b[31m${msg.error}\x1b[0m`);
						return;
					}
				} catch {
					// Not JSON, treat as terminal output
				}
			}

			term?.write(data);
		};

		ws.onclose = () => {
			if (status === 'connected') {
				term?.writeln('\r\n\x1b[33mConnection closed.\x1b[0m');
			}
			status = 'disconnected';
		};

		ws.onerror = () => {
			status = 'disconnected';
		};
	}

	function reconnect() {
		ws?.close();
		term?.clear();
		connect();
	}
</script>

<div class="flex h-[calc(100vh-4rem)] flex-col">
	<div class="mb-4 flex shrink-0 items-center gap-3">
		<span class="text-xs uppercase {
			status === 'connected' ? 'text-green-400' :
			status === 'connecting' ? 'text-amber-500' : 'text-muted-foreground'
		}">{status}</span>
		{#if status === 'disconnected'}
			<Button size="sm" onclick={reconnect}>Reconnect</Button>
		{/if}
	</div>
	<div class="flex-1 overflow-hidden rounded-lg border border-border p-1" style="background: #0f1117" bind:this={terminalEl}></div>
</div>

<style>
	@import '@xterm/xterm/css/xterm.css';

	div :global(.xterm) {
		height: 100%;
	}

	div :global(.xterm-viewport) {
		overflow-y: auto !important;
	}
</style>
