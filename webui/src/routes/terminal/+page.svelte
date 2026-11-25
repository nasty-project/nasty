<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { Terminal } from '@xterm/xterm';
	import { FitAddon } from '@xterm/addon-fit';
	import { WebLinksAddon } from '@xterm/addon-web-links';
	import { Button } from '$lib/components/ui/button';
	import { Maximize2, Minimize2 } from '@lucide/svelte';
	import { terminalStatus } from '$lib/terminalStatus.svelte';
	import { confirm } from '$lib/confirm.svelte';
	import { nextFreeShell, pickShell } from '$lib/terminal-shells';

	import type { IDisposable } from '@xterm/xterm';

	let terminalEl: HTMLDivElement;
	let term: Terminal | null = null;
	let fitAddon: FitAddon | null = null;
	let ws: WebSocket | null = null;
	let status = $state<'connecting' | 'connected' | 'disconnected'>('connecting');

	// Sync local status to shared store for top bar indicator
	$effect(() => { terminalStatus.set(status); });
	let termListeners: IDisposable[] = [];
	let fullscreen = $state(false);

	// Persistent shells (#1085). A `?cmd=` link runs in a one-off shell so it
	// never types into a job running in a persistent one.
	const SHELL_KEY = 'nasty-terminal-shell';
	const oneOff = new URLSearchParams(window.location.search).has('cmd');
	let shells = $state<number[]>([]);
	let maxShells = $state(4);
	let current = $state<number | null>(null);
	let shellError = $state('');
	const tabs = $derived([...new Set([...shells, ...(current === null ? [] : [current])])].sort((a, b) => a - b));

	async function loadShells(): Promise<boolean> {
		try {
			const res = await fetch('/api/terminal/shells');
			if (!res.ok) return false;
			const body = await res.json();
			shells = body.shells;
			maxShells = body.max;
			return true;
		} catch {
			return false;
		}
	}

	function rememberedShell(): number | null {
		try {
			const n = Number(localStorage.getItem(SHELL_KEY));
			return Number.isInteger(n) && n > 0 ? n : null;
		} catch {
			return null;
		}
	}

	function openShell(n: number) {
		current = n;
		try { localStorage.setItem(SHELL_KEY, String(n)); } catch { /* per-viewer convenience only */ }
		reconnect();
	}

	function newShell() {
		const n = nextFreeShell(tabs, maxShells);
		if (n !== null) openShell(n);
	}

	async function closeShell() {
		if (current === null) return;
		const n = current;
		if (!await confirm(`Close shell ${n}?`, 'Anything still running in it will be stopped.', { confirmLabel: 'Close shell' })) return;
		shellError = '';
		const res = await fetch(`/api/terminal/shells/${n}`, { method: 'DELETE' }).catch(() => null);
		if (!res?.ok) {
			shellError = `Could not close shell ${n}.`;
			return;
		}
		await loadShells();
		openShell(pickShell(shells, null));
	}

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

		if (oneOff) {
			connect();
		} else {
			// An engine without persistent shells has no list: open a one-off shell.
			void loadShells().then((supported) => {
				current = supported ? pickShell(shells, rememberedShell()) : null;
				connect();
			});
		}

		const resizeObserver = new ResizeObserver(() => {
			fitAddon?.fit();
		});
		resizeObserver.observe(terminalEl);

		return () => {
			resizeObserver.disconnect();
		};
	});

	onDestroy(() => {
		const socket = ws;
		ws = null; // leaving the page is not a dropped connection
		socket?.close();
		term?.dispose();
	});

	function connect() {
		// Cookie auth: the WS upgrade carries `nasty_session` automatically;
		// the first message just supplies terminal size.
		const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
		ws = new WebSocket(`${proto}//${window.location.host}/ws/terminal`);
		status = 'connecting';

		ws.onopen = () => {
			const cols = term?.cols ?? 80;
			const rows = term?.rows ?? 24;
			ws?.send(JSON.stringify(current === null ? { cols, rows } : { cols, rows, shell: current }));
		};

		ws.onmessage = (event) => {
			const data = event.data;

			if (status === 'connecting') {
				try {
					const msg = JSON.parse(data);
					if (msg.authenticated) {
						status = 'connected';
						if (current !== null) void loadShells();

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

						// Send initial command from URL parameter (e.g. app shell)
						const cmdParam = new URLSearchParams(window.location.search).get('cmd');
						if (cmdParam && ws?.readyState === WebSocket.OPEN) {
							ws.send(cmdParam + '\n');
						}

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

		const socket = ws;
		ws.onclose = async () => {
			// A switch to another shell closes this socket on purpose.
			if (ws !== socket) return;
			const wasConnected = status === 'connected';
			status = 'disconnected';
			if (!wasConnected) return;
			if (current !== null && await loadShells() && !shells.includes(current)) {
				term?.writeln('\r\n\x1b[33mShell ended.\x1b[0m');
			} else {
				term?.writeln('\r\n\x1b[33mConnection closed.\x1b[0m');
			}
		};

		ws.onerror = () => {
			if (ws === socket) status = 'disconnected';
		};
	}

	function reconnect() {
		ws?.close();
		term?.reset();
		connect();
	}
</script>

<div class="{fullscreen ? 'fixed inset-0 z-50 flex flex-col bg-[#0f1117]' : 'flex h-full flex-col'}">
	{#if current !== null}
		<div class="mb-2 flex flex-wrap items-center gap-2 {fullscreen ? 'px-2 pt-2' : ''}" role="tablist" aria-label="Shells">
			{#each tabs as n (n)}
				<Button size="sm" variant={n === current ? 'default' : 'secondary'} role="tab" aria-selected={n === current}
					onclick={() => { if (n !== current) openShell(n); }}>Shell {n}</Button>
			{/each}
			{#if nextFreeShell(tabs, maxShells) !== null}
				<Button size="sm" variant="secondary" onclick={newShell}>New shell</Button>
			{/if}
			<Button size="sm" variant="secondary" onclick={closeShell}>Close shell {current}</Button>
			<span class="text-xs text-muted-foreground">Shells keep running when you leave this page.</span>
			{#if shellError}<span role="alert" class="text-xs text-red-400">{shellError}</span>{/if}
		</div>
	{/if}
	<div class="relative flex-1 min-h-0 overflow-hidden {fullscreen ? '' : 'rounded-lg border border-border'} p-1" style="background: #0f1117">
		<div bind:this={terminalEl} class="h-full w-full"></div>
		<div class="absolute top-2 right-2 flex items-center gap-2 z-10">
			{#if status === 'disconnected'}
				<Button size="sm" onclick={reconnect}>Reconnect</Button>
			{/if}
			<button
				onclick={() => { fullscreen = !fullscreen; setTimeout(() => fitAddon?.fit(), 0); }}
				class="flex items-center rounded-md bg-black/50 p-1.5 text-muted-foreground/60 transition-colors hover:bg-black/80 hover:text-foreground"
				title={fullscreen ? 'Exit fullscreen' : 'Fullscreen'}
			>
				{#if fullscreen}
					<Minimize2 size={14} />
				{:else}
					<Maximize2 size={14} />
				{/if}
			</button>
		</div>
	</div>
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
