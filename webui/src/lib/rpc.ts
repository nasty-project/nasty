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
	 *  cap, resets to the floor on a successful auth. Two profiles:
	 *
	 *    `normal`     — the default. Used for the "engine just rebooted /
	 *                   network blip" case. Backoff sequence to reload:
	 *                   1+2+4+5+5+5+5+5+5+5 = ~42 s.
	 *    `aggressive` — flipped on by [`setAggressiveReconnect`] while a
	 *                   known restart is in flight (the only caller today
	 *                   is the Update page, which knows the engine is
	 *                   coming down and back during an upgrade). Backoff
	 *                   sequence to reload: 0.25+0.5+1+1.5×17 ≈ ~27 s.
	 *
	 *  The earlier shape used a 30 s ceiling. That was fine in theory but
	 *  meant up to a 30 s gap between "engine is back" and "WebUI tries
	 *  the next WS handshake" — operators watching an Upgrade progress
	 *  bar perceived the lag as the WebUI being broken even though the
	 *  reconnect was healthy. A few-seconds ceiling keeps the WS attempt
	 *  rate cheap enough on the server side (~1 attempt every couple of
	 *  seconds during outage) and the user-perceived lag near zero. */
	private reconnectDelayMs = 1000;
	private static readonly NORMAL_DELAY_FLOOR_MS = 1000;
	private static readonly NORMAL_DELAY_CEIL_MS = 5_000;
	private static readonly NORMAL_MAX_ATTEMPTS_BEFORE_RELOAD = 10;
	private static readonly AGGRESSIVE_DELAY_FLOOR_MS = 250;
	private static readonly AGGRESSIVE_DELAY_CEIL_MS = 1_500;
	private static readonly AGGRESSIVE_MAX_ATTEMPTS_BEFORE_RELOAD = 20;
	/** Aggressive-reconnect toggle — see [`setAggressiveReconnect`]. */
	private _aggressive = false;
	/** Number of consecutive failed reconnect attempts since the last
	 *  successful auth. Drives the page-reload escape hatch below. */
	private consecutiveFailedReconnects = 0;

	/** Lower bound for the next backoff delay, mode-dependent. */
	private get reconnectFloorMs(): number {
		return this._aggressive
			? NastyClient.AGGRESSIVE_DELAY_FLOOR_MS
			: NastyClient.NORMAL_DELAY_FLOOR_MS;
	}

	/** Upper bound for the next backoff delay, mode-dependent. */
	private get reconnectCeilMs(): number {
		return this._aggressive
			? NastyClient.AGGRESSIVE_DELAY_CEIL_MS
			: NastyClient.NORMAL_DELAY_CEIL_MS;
	}

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
	 *  browser-level error state instead of a spinner. */
	private get maxAttemptsBeforeReload(): number {
		return this._aggressive
			? NastyClient.AGGRESSIVE_MAX_ATTEMPTS_BEFORE_RELOAD
			: NastyClient.NORMAL_MAX_ATTEMPTS_BEFORE_RELOAD;
	}
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
						this.reconnectDelayMs = this.reconnectFloorMs;
						this.consecutiveFailedReconnects = 0;
						this._readyResolve?.();
						this._readyResolve = null;
						if (NastyClient.debug) {
							console.debug(
								`[rpc] auth ok (wasReconnect=${wasReconnect}, firing ${this.reconnectHandlers.length} reconnect handlers)`
							);
						}
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

			this.ws.onclose = (ev) => {
				this._authenticated = false;
				// Reject all pending calls so awaiting code doesn't hang forever
				for (const pending of this.pending.values()) {
					pending.reject({ code: -32000, message: 'WebSocket disconnected' });
				}
				this.pending.clear();
				if (NastyClient.debug) {
					console.debug(
						`[rpc] ws closed (code=${ev.code}, reason="${ev.reason}", clean=${ev.wasClean}, _shouldReconnect=${this._shouldReconnect}, firing ${this._shouldReconnect ? this.disconnectHandlers.length : 0} disconnect handlers)`
					);
				}
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
			this.reconnectCeilMs,
		);
		if (NastyClient.debug) {
			console.debug(
				`[rpc] scheduling reconnect in ${delay}ms (next ceiling: ${this.reconnectDelayMs}ms, consecutive failures: ${this.consecutiveFailedReconnects}, aggressive=${this._aggressive})`
			);
		}
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
					if (this.consecutiveFailedReconnects >= this.maxAttemptsBeforeReload) {
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

	/** Tell the client a known restart is in flight so reconnect should
	 *  be more aggressive. Today only the Update page calls this — between
	 *  Upgrade-click and `system.update.status` transitioning to
	 *  success/failed it flips this on, then flips it off again.
	 *
	 *  Effects while `true`:
	 *    - Backoff floor 250 ms / ceiling 1.5 s (vs. 1 s / 5 s).
	 *    - Reload escape hatch trips after ~20 attempts (~27 s) instead of
	 *      ~10 attempts (~42 s). Sized so an upgrade that genuinely fails
	 *      to bring the engine back drops to the login screen quickly,
	 *      while a healthy ~20–60 s activation window still recovers
	 *      cleanly without ever tripping reload.
	 *    - If we're already mid-backoff when flipped on, the next scheduled
	 *      retry happens at the aggressive ceiling rather than waiting out
	 *      the current (possibly multi-second) timer.
	 *
	 *  Idempotent — calling with the same value twice is a no-op. */
	setAggressiveReconnect(active: boolean) {
		if (this._aggressive === active) return;
		this._aggressive = active;
		if (NastyClient.debug) {
			console.debug(
				`[rpc] setAggressiveReconnect(${active}) — floor=${this.reconnectFloorMs}ms, ceiling=${this.reconnectCeilMs}ms`
			);
		}
		// If we just entered aggressive mode mid-outage, the current
		// reconnectDelayMs may already be at the old (5 s / 30 s) ceiling.
		// Snap it down so the very next scheduled retry fires fast.
		// (Doesn't cancel an already-pending timer — that's fine, the
		// timer's delay was set when it was scheduled; the snap takes
		// effect for the *next* one. In practice the next retry is
		// already milliseconds away because we're typically in this code
		// path because a reconnect just failed.)
		if (active) {
			this.reconnectDelayMs = Math.min(
				this.reconnectDelayMs,
				NastyClient.AGGRESSIVE_DELAY_CEIL_MS,
			);
		}
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
