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
