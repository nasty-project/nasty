import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest';
import { NastyClient } from './rpc';

// ── Mock WebSocket ──────────────────────────────────────────────────
// Tests drive the lifecycle synchronously: construct, then call open(),
// receive(...), fireClose(), fireError() to simulate server activity.

let mockInstances: MockWebSocket[] = [];

class MockWebSocket {
	static CONNECTING = 0;
	static OPEN = 1;
	static CLOSING = 2;
	static CLOSED = 3;

	readyState = MockWebSocket.CONNECTING;
	sent: string[] = [];

	onopen: ((e: Event) => void) | null = null;
	onmessage: ((e: MessageEvent) => void) | null = null;
	onclose: ((e: CloseEvent) => void) | null = null;
	onerror: ((e: Event) => void) | null = null;

	constructor(public url: string) {
		mockInstances.push(this);
	}

	send(data: string) {
		this.sent.push(data);
	}

	// Production code calls this in disconnect(); fire onclose synchronously
	// so tests can verify cleanup without awaiting.
	close() {
		this.readyState = MockWebSocket.CLOSED;
		this.onclose?.({} as CloseEvent);
	}

	// ── Test driver helpers ───────────────────────────────────────
	open() {
		this.readyState = MockWebSocket.OPEN;
		this.onopen?.({} as Event);
	}

	receive(data: unknown) {
		this.onmessage?.({ data: JSON.stringify(data) } as MessageEvent);
	}

	fireClose() {
		this.readyState = MockWebSocket.CLOSED;
		this.onclose?.({} as CloseEvent);
	}

	fireError() {
		this.onerror?.({} as Event);
	}

	sentRequest(idx = 0): { jsonrpc: string; method: string; params?: unknown; id: number } {
		return JSON.parse(this.sent[idx]);
	}
}

async function connectAuthed(): Promise<NastyClient> {
	const client = new NastyClient('ws://localhost/api');
	const promise = client.connect();
	const mock = mockInstances[mockInstances.length - 1];
	mock.open();
	mock.receive({ authenticated: true, username: 'admin', role: 'admin' });
	await promise;
	return client;
}

beforeEach(() => {
	mockInstances = [];
	vi.stubGlobal('WebSocket', MockWebSocket);
});

afterEach(() => {
	vi.unstubAllGlobals();
});

// ── Auth handshake ──────────────────────────────────────────────────

describe('auth handshake', () => {
	test('connect resolves with AuthResult on success', async () => {
		const client = new NastyClient('ws://localhost/api');
		const promise = client.connect();
		const mock = mockInstances[0];
		mock.open();
		mock.receive({ authenticated: true, username: 'admin', role: 'admin' });
		const result = await promise;
		expect(result.username).toBe('admin');
		expect(result.role).toBe('admin');
		expect(client.authenticated).toBe(true);
	});

	test('connect rejects when server returns an error', async () => {
		const client = new NastyClient('ws://localhost/api');
		const promise = client.connect();
		const mock = mockInstances[0];
		mock.open();
		mock.receive({ error: 'Invalid credentials' });
		await expect(promise).rejects.toThrow('Invalid credentials');
		expect(client.authenticated).toBe(false);
	});

	test('connect rejects on unexpected first message', async () => {
		const client = new NastyClient('ws://localhost/api');
		const promise = client.connect();
		const mock = mockInstances[0];
		mock.open();
		mock.receive({ result: 'something else' });
		await expect(promise).rejects.toThrow(/Unexpected auth response/);
	});

	test('connect rejects when WebSocket onerror fires before auth', async () => {
		const client = new NastyClient('ws://localhost/api');
		const promise = client.connect();
		mockInstances[0].fireError();
		await expect(promise).rejects.toThrow(/WebSocket connection failed/);
	});
});

// ── Request / response correlation ──────────────────────────────────

describe('request/response correlation', () => {
	test('call sends a JSON-RPC 2.0 request and returns the matching result', async () => {
		const client = await connectAuthed();
		const mock = mockInstances[0];
		const callPromise = client.call('fs.list');
		const sent = mock.sentRequest(0);
		expect(sent.jsonrpc).toBe('2.0');
		expect(sent.method).toBe('fs.list');
		expect(typeof sent.id).toBe('number');
		mock.receive({ jsonrpc: '2.0', id: sent.id, result: ['tank', 'pool2'] });
		await expect(callPromise).resolves.toEqual(['tank', 'pool2']);
	});

	test('call rejects with the server error envelope', async () => {
		const client = await connectAuthed();
		const mock = mockInstances[0];
		const callPromise = client.call('fs.delete', { name: 'missing' });
		const sent = mock.sentRequest(0);
		mock.receive({
			jsonrpc: '2.0',
			id: sent.id,
			error: { code: -32601, message: 'no such filesystem' }
		});
		await expect(callPromise).rejects.toMatchObject({
			code: -32601,
			message: 'no such filesystem'
		});
	});

	test('concurrent calls route responses by id even when replies arrive out of order', async () => {
		const client = await connectAuthed();
		const mock = mockInstances[0];
		const a = client.call<string>('a');
		const b = client.call<string>('b');
		const sentA = mock.sentRequest(0);
		const sentB = mock.sentRequest(1);
		expect(sentA.id).not.toBe(sentB.id);
		// Reply to b first, then a — must still route correctly.
		mock.receive({ jsonrpc: '2.0', id: sentB.id, result: 'B' });
		mock.receive({ jsonrpc: '2.0', id: sentA.id, result: 'A' });
		await expect(a).resolves.toBe('A');
		await expect(b).resolves.toBe('B');
	});

	test('call rejects with timeout when no reply arrives', async () => {
		vi.useFakeTimers();
		try {
			const client = await connectAuthed();
			const callPromise = client.call('slow', undefined, 100);
			const expectation = expect(callPromise).rejects.toMatchObject({
				message: 'Request timed out'
			});
			await vi.advanceTimersByTimeAsync(150);
			await expectation;
		} finally {
			vi.useRealTimers();
		}
	});

	test('call before authentication rejects', async () => {
		const client = new NastyClient('ws://localhost/api');
		await expect(client.call('anything')).rejects.toThrow(/Not connected or not authenticated/);
	});
});

// ── Server-pushed notifications ─────────────────────────────────────

describe('notifications', () => {
	test('onEvent fires with method and params for messages without id', async () => {
		const client = await connectAuthed();
		const handler = vi.fn();
		client.onEvent(handler);
		mockInstances[0].receive({
			jsonrpc: '2.0',
			method: 'alerts.changed',
			params: { count: 3 }
		});
		expect(handler).toHaveBeenCalledWith('alerts.changed', { count: 3 });
	});

	test('offEvent removes the handler', async () => {
		const client = await connectAuthed();
		const handler = vi.fn();
		client.onEvent(handler);
		client.offEvent(handler);
		mockInstances[0].receive({ jsonrpc: '2.0', method: 'x', params: {} });
		expect(handler).not.toHaveBeenCalled();
	});
});

// ── Disconnect / reconnect ──────────────────────────────────────────

describe('connection lifecycle', () => {
	test('pending calls reject with disconnect error when WS closes', async () => {
		const client = await connectAuthed();
		const callPromise = client.call('slow');
		mockInstances[0].fireClose();
		await expect(callPromise).rejects.toMatchObject({
			code: -32000,
			message: 'WebSocket disconnected'
		});
	});

	test('onDisconnect fires when the connection drops after authentication', async () => {
		const client = await connectAuthed();
		const handler = vi.fn();
		client.onDisconnect(handler);
		mockInstances[0].fireClose();
		expect(handler).toHaveBeenCalled();
	});

	test('disconnect() clears auth and never reconnects', async () => {
		const client = await connectAuthed();
		expect(client.authenticated).toBe(true);
		client.disconnect();
		expect(client.authenticated).toBe(false);
		// No second WebSocket gets constructed — disconnect is one-shot.
		expect(mockInstances).toHaveLength(1);
	});

	test('reconnect is scheduled on drop and onReconnect fires on re-auth', async () => {
		vi.useFakeTimers();
		try {
			const client = new NastyClient('ws://localhost/api');
			const initial = client.connect();
			mockInstances[0].open();
			mockInstances[0].receive({ authenticated: true, username: 'admin', role: 'admin' });
			await initial;

			const reconnectHandler = vi.fn();
			client.onReconnect(reconnectHandler);

			// Server drops the connection.
			mockInstances[0].fireClose();
			expect(mockInstances).toHaveLength(1); // not yet reconnected

			// _scheduleReconnect uses a 3000ms timer.
			await vi.advanceTimersByTimeAsync(3000);
			expect(mockInstances).toHaveLength(2);

			// Drive the second auth handshake.
			mockInstances[1].open();
			mockInstances[1].receive({ authenticated: true, username: 'admin', role: 'admin' });

			expect(reconnectHandler).toHaveBeenCalled();
			expect(client.authenticated).toBe(true);
		} finally {
			vi.useRealTimers();
		}
	});
});
