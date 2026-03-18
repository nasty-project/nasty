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
}

export class NastyClient {
	private ws: WebSocket | null = null;
	private nextId = 1;
	private pending = new Map<number, PendingCall>();
	private eventHandlers: EventHandler[] = [];
	private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
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

	/** Connect and authenticate with a token */
	connect(token: string): Promise<AuthResult> {
		return new Promise((resolve, reject) => {
			this.ws = new WebSocket(this.url);
			let authResolved = false;

			this.ws.onopen = () => {
				// Send token as first message
				this.ws!.send(JSON.stringify({ token }));
			};

			this.ws.onerror = () => {
				if (!authResolved) reject(new Error('WebSocket connection failed'));
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
						this._authenticated = true;
						this._shouldReconnect = true;
						this._readyResolve?.();
						this._readyResolve = null;
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
					// Prepare a new ready-promise so calls made while reconnecting will wait.
					this._readyPromise = new Promise((res) => { this._readyResolve = res; });
					this.reconnectTimer = setTimeout(() => this.connect(token).catch(() => {}), 3000);
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

		return new Promise<T>((resolve, reject) => {
			const timer = setTimeout(() => {
				this.pending.delete(id);
				reject({ code: -32000, message: 'Request timed out' });
			}, timeoutMs);

			this.pending.set(id, {
				resolve: (v) => { clearTimeout(timer); resolve(v as T); },
				reject: (e) => { clearTimeout(timer); reject(e); }
			});
			this.ws!.send(JSON.stringify(request));
		});
	}

	onEvent(handler: EventHandler) {
		this.eventHandlers.push(handler);
	}

	offEvent(handler: EventHandler) {
		this.eventHandlers = this.eventHandlers.filter((h) => h !== handler);
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
