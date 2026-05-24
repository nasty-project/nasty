/** JSON-RPC 2.0 client over WebSocket with token auth */

interface RpcError {
	code: number;
	message: string;
	data?: unknown;
}

interface PendingCall {
	resolve: (value: unknown) => void;
	reject: (error: RpcError) => void;
}

export type EventHandler = (method: string, params: unknown) => void;

export interface AuthResult {
	authenticated: boolean;
	username: string;
	role: string;
	must_change_password?: boolean;
}

export class NastyClient {
	private ws: WebSocket | null = null;
	private nextId = 1;
	private pending = new Map<number, PendingCall>();
	private eventHandlers: EventHandler[] = [];
	private reconnectHandlers: (() => void)[] = [];
	private disconnectHandlers: (() => void)[] = [];
	private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	/** Exponential reconnect delay — doubles on each failed attempt up to a
	 *  cap, resets to the floor on a successful auth. The previous version
	 *  used a fixed 3 s delay, which during a longer outage produced 20
	 *  attempts per minute — friendlier to spam the server slower and
	 *  faster to reconnect from a transient blip. */
	private reconnectDelayMs = 1000;
	private static readonly RECONNECT_DELAY_FLOOR_MS = 1000;
	private static readonly RECONNECT_DELAY_CEIL_MS = 30_000;
	/** Number of consecutive failed reconnect attempts since the last
	 *  successful auth. Drives the page-reload escape hatch below. */
	private consecutiveFailedReconnects = 0;
	/** After this many consecutive failed reconnect attempts, force a
	 *  full page reload instead of scheduling yet another retry.
	 *
	 *  Why this exists: a WebSocket TLS handshake can fail silently in
	 *  the browser (most commonly when the box rebooted and Caddy
	 *  re-issued a self-signed leaf with a new fingerprint — the prior
	 *  browser exception is keyed to the old fingerprint and the new
	 *  one is silently rejected with no cert-warning UI, because the
	 *  WebSocket API doesn't surface that prompt). The reconnect loop
	 *  then spins forever — "Reconnecting..." with no progress.
	 *  `location.reload()` does a full HTML navigation which DOES
	 *  surface the cert-warning UI; once the operator clicks through,
	 *  the WS connects fine on the reloaded page. The same escape
	 *  hatch also unblocks "box has moved to a new IP", "server
	 *  config changed in a way that breaks the WS endpoint", and any
	 *  other persistent reconnect failure — the user gets a clear
	 *  browser-level error state instead of a spinner.
	 *
	 *  Threshold sized so a normal reboot (~30–90 s downtime) recovers
	 *  via the normal reconnect path without ever tripping reload, but
	 *  a stuck state unblocks within ~3 minutes. Backoff totals at
	 *  the 10th attempt: 1+2+4+8+16+30+30+30+30+30 = 181 s. */
	private static readonly MAX_RECONNECT_ATTEMPTS_BEFORE_RELOAD = 10;
	private _authenticated = false;
	/** Set to true after the first successful auth; cleared by disconnect(). */
	private _shouldReconnect = false;
	/** Resolves when the next successful auth completes; replaced on each disconnect. */
	private _readyResolve: (() => void) | null = null;
	private _readyPromise: Promise<void> = Promise.resolve();

	constructor(private url: string) {}

	get authenticated() {
		return this._authenticated;
	}

	/** Connect and authenticate. The session cookie set by /api/login is sent
	 *  automatically with the WS upgrade; the client doesn't send any auth
	 *  message itself. The server replies first with `{authenticated: true}`
	 *  or `{error: ...}`. */
	connect(): Promise<AuthResult> {
		return new Promise((resolve, reject) => {
			this.ws = new WebSocket(this.url);
			let authResolved = false;

			this.ws.onopen = () => {
				// Cookie auth: nothing to send on open. Server speaks first.
			};

			this.ws.onmessage = (event) => {
				const msg = JSON.parse(event.data);

				// Handle auth response (first message back)
				if (!authResolved) {
					authResolved = true;
					if (msg.error) {
						this._authenticated = false;
						reject(new Error(msg.error));
					} else if (msg.authenticated) {
						const wasReconnect = this._shouldReconnect;
						this._authenticated = true;
						this._shouldReconnect = true;
						// Successful connect — reset the backoff so the next
						// disconnect retries quickly, and clear the
						// failed-reconnect counter so the reload escape hatch
						// only fires after a fresh streak of failures.
						this.reconnectDelayMs = NastyClient.RECONNECT_DELAY_FLOOR_MS;
						this.consecutiveFailedReconnects = 0;
						this._readyResolve?.();
						this._readyResolve = null;
						if (wasReconnect) {
							for (const h of this.reconnectHandlers) h();
						}
						resolve(msg as AuthResult);
					} else {
						reject(new Error('Unexpected auth response'));
					}
					return;
				}

				if ('id' in msg && msg.id !== null) {
					const pending = this.pending.get(msg.id);
					if (pending) {
						this.pending.delete(msg.id);
						if (msg.error) {
							pending.reject(msg.error);
						} else {
							pending.resolve(msg.result);
						}
					}
				} else if ('method' in msg) {
					for (const handler of this.eventHandlers) {
						handler(msg.method, msg.params);
					}
				}
			};

			this.ws.onclose = () => {
				this._authenticated = false;
				// Reject all pending calls so awaiting code doesn't hang forever
				for (const pending of this.pending.values()) {
					pending.reject({ code: -32000, message: 'WebSocket disconnected' });
				}
				this.pending.clear();
				// Keep retrying as long as we haven't been explicitly disconnected.
				if (this._shouldReconnect) {
					for (const h of this.disconnectHandlers) h();
					this._scheduleReconnect();
				}
			};

			this.ws.onerror = () => {
				if (!authResolved) reject(new Error('WebSocket connection failed'));
				// If this was a reconnect attempt that failed to even open,
				// onclose may not fire, so schedule retry here too.
				if (this._shouldReconnect && !this._authenticated) {
					this._scheduleReconnect();
				}
			};
		});
	}

	async call<T = unknown>(method: string, params?: unknown, timeoutMs = 10000): Promise<T> {
		// If mid-reconnect, wait for the connection to come back rather than failing immediately.
		if (!this._authenticated && this._shouldReconnect) {
			await this._readyPromise;
		}

		if (!this.ws || this.ws.readyState !== WebSocket.OPEN || !this._authenticated) {
			throw new Error('Not connected or not authenticated');
		}

		const id = this.nextId++;
		const request = {
			jsonrpc: '2.0',
			method,
			params: params ?? undefined,
			id
		};

		const t0 = NastyClient.debug ? performance.now() : 0;
		return new Promise<T>((resolve, reject) => {
			const timer = setTimeout(() => {
				this.pending.delete(id);
				reject({ code: -32000, message: 'Request timed out' });
			}, timeoutMs);

			this.pending.set(id, {
				resolve: (v) => {
					clearTimeout(timer);
					if (NastyClient.debug) {
						console.debug(`[rpc] ${method}: ${(performance.now() - t0).toFixed(0)}ms`);
					}
					resolve(v as T);
				},
				reject: (e) => { clearTimeout(timer); reject(e); }
			});
			this.ws!.send(JSON.stringify(request));
		});
	}

	/** Schedule a reconnection attempt. Deduplicates to avoid multiple timers.
	 *  Uses an exponential backoff: starts at RECONNECT_DELAY_FLOOR_MS, doubles
	 *  on each failed attempt up to RECONNECT_DELAY_CEIL_MS, and resets to the
	 *  floor on a successful reconnect (see onmessage). Faster reconnect from
	 *  transient blips than the old fixed 3 s, and friendlier to the server
	 *  during a longer outage (a stuck client used to spam ~20 attempts/min). */
	private _scheduleReconnect() {
		if (this.reconnectTimer) return; // already scheduled
		this._readyPromise = new Promise((res) => { this._readyResolve = res; });
		const delay = this.reconnectDelayMs;
		this.reconnectDelayMs = Math.min(
			this.reconnectDelayMs * 2,
			NastyClient.RECONNECT_DELAY_CEIL_MS,
		);
		this.reconnectTimer = setTimeout(() => {
			this.reconnectTimer = null;
			this.connect().catch((err) => {
				// Auth failure after reboot (cookie invalidated) — force page reload.
				// The new page will show the login form.
				if (err instanceof Error && (
					err.message.includes('Invalid') ||
					err.message.includes('Unauthorized') ||
					err.message.includes('expired')
				)) {
					location.reload();
					return;
				}
				// Connection failed (server still down, or silent TLS reject
				// after leaf-cert rotation, or any other persistent failure).
				// After enough attempts, reload the page — see
				// MAX_RECONNECT_ATTEMPTS_BEFORE_RELOAD for the rationale.
				if (this._shouldReconnect) {
					this.consecutiveFailedReconnects += 1;
					if (
						this.consecutiveFailedReconnects
						>= NastyClient.MAX_RECONNECT_ATTEMPTS_BEFORE_RELOAD
					) {
						location.reload();
						return;
					}
					this._scheduleReconnect();
				}
			});
		}, delay);
	}

	/** Enable with localStorage.setItem('nasty-debug', '1') then reload */
	static debug = typeof localStorage !== 'undefined' && localStorage.getItem('nasty-debug') === '1';

	onEvent(handler: EventHandler) {
		this.eventHandlers.push(handler);
	}

	offEvent(handler: EventHandler) {
		this.eventHandlers = this.eventHandlers.filter((h) => h !== handler);
	}

	/** Called whenever the client successfully reconnects after a dropped connection. */
	onReconnect(handler: () => void) {
		this.reconnectHandlers.push(handler);
	}

	offReconnect(handler: () => void) {
		this.reconnectHandlers = this.reconnectHandlers.filter((h) => h !== handler);
	}

	/** Called when the connection drops and auto-reconnect begins. */
	onDisconnect(handler: () => void) {
		this.disconnectHandlers.push(handler);
	}

	offDisconnect(handler: () => void) {
		this.disconnectHandlers = this.disconnectHandlers.filter((h) => h !== handler);
	}

	disconnect() {
		this._shouldReconnect = false;
		this._readyResolve = null;
		this._readyPromise = Promise.resolve();
		if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
		this._authenticated = false;
		this.ws?.close();
	}
}
