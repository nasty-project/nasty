// Auth is cookie-based: the engine sets `nasty_session` as an httpOnly,
// Secure, SameSite=Strict cookie on /api/login and /api/auth/oidc/callback.
// JS can't read the cookie, so XSS can't steal the session. Same-origin
// fetches and same-origin WS upgrades carry it automatically — there is no
// JS-visible token to thread through requests.

export async function login(username: string, password: string): Promise<void> {
	let res: Response;
	try {
		res = await fetch('/api/login', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ username, password }),
		});
	} catch (e) {
		// Network-layer failure (DNS, TLS, server unreachable) — the
		// browser couldn't even get a response. Distinct from a 5xx
		// because nothing on the other end answered at all.
		const msg = e instanceof Error ? e.message : String(e);
		throw new Error(`Can't reach server: ${msg}`);
	}

	if (res.ok) {
		// Token is in the response body (for CLI tools that don't have
		// a cookie jar) but the WebUI doesn't read it — the Set-Cookie
		// header is the source of truth for the browser.
		return;
	}

	// Map status classes onto messages an operator can act on. Without
	// this split, an engine that's down/timing out looked identical to
	// a typo'd password — both surfaced as a bare "Login failed."
	if (res.status >= 500) {
		throw new Error(
			`NASty engine unavailable (HTTP ${res.status}). The backend isn't responding — check 'systemctl status nasty-engine' on the box.`
		);
	}
	if (res.status === 503 || res.status === 504) {
		// Caddy uses these when the upstream is unreachable / slow.
		// (Covered by the >=500 branch above too, but keep the specific
		// hint for the common gateway cases.)
		throw new Error(`NASty engine isn't accepting requests yet (HTTP ${res.status}). Retry in a moment.`);
	}

	// 4xx — the request reached the engine and was refused. Honor the
	// engine's error message when present (e.g. "invalid credentials"),
	// otherwise fall back to a status-aware default.
	const body = await res.json().catch(() => ({}));
	const detail = (body as { error?: string }).error;
	if (detail) {
		throw new Error(detail);
	}
	if (res.status === 401 || res.status === 403) {
		throw new Error('Invalid username or password.');
	}
	throw new Error(`Login failed (HTTP ${res.status}).`);
}

export async function logout(): Promise<void> {
	// Browsers can't delete an httpOnly cookie themselves, so logout has to
	// round-trip through the server: it revokes the session record and sends
	// Set-Cookie with Max-Age=0 to drop the cookie.
	await fetch('/api/logout', { method: 'POST' }).catch(() => {});
}

/** Sign in via a registered WebAuthn credential (issue #289 PR #2).
 * The browser side of the ceremony — fetch a challenge from the
 * engine, hand it to @simplewebauthn's `startAuthentication`, post
 * the resulting assertion back to the engine, which mints a
 * `nasty_session` cookie just like the password path. On any
 * failure (no creds registered, user dismissed the prompt, wrong
 * key, etc.) the caller catches and surfaces the message. */
export async function loginWebauthn(username: string): Promise<void> {
	const { startAuthentication } = await import('@simplewebauthn/browser');
	let startRes: Response;
	try {
		startRes = await fetch('/api/auth/webauthn/login/start', {
			method: 'POST',
			headers: { 'Content-Type': 'application/json' },
			body: JSON.stringify({ username }),
		});
	} catch (e) {
		const msg = e instanceof Error ? e.message : String(e);
		throw new Error(`Can't reach server: ${msg}`);
	}
	if (!startRes.ok) {
		const body = await startRes.json().catch(() => ({}));
		const detail = (body as { error?: string }).error;
		throw new Error(
			detail
				?? (startRes.status === 401 || startRes.status === 403
					? 'No security keys registered for this account.'
					: `Login failed (HTTP ${startRes.status}).`),
		);
	}
	const { auth_id, request_options } = await startRes.json();

	// simplewebauthn handles the JSON ↔ ArrayBuffer dance. The
	// engine sends webauthn-rs's `RequestChallengeResponse` shape
	// directly; same JSON form simplewebauthn accepts.
	let response;
	try {
		response = await startAuthentication({
			optionsJSON: (request_options as { publicKey?: unknown }).publicKey ?? request_options,
		} as Parameters<typeof startAuthentication>[0]);
	} catch (e) {
		// Most common: NotAllowedError (user dismissed prompt, wrong
		// key, no matching credential on the authenticator). Short
		// message is friendlier than the raw DOMException string.
		const msg = e instanceof Error ? e.message : String(e);
		throw new Error(`Security key prompt failed: ${msg}`);
	}

	const finishRes = await fetch('/api/auth/webauthn/login/finish', {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify({ username, auth_id, response }),
	});
	if (!finishRes.ok) {
		const body = await finishRes.json().catch(() => ({}));
		const detail = (body as { error?: string }).error;
		throw new Error(detail ?? `Login failed (HTTP ${finishRes.status}).`);
	}
	// Session cookie is set by the engine. Caller's job is to refresh
	// the connection.
}
