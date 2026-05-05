// Auth is cookie-based: the engine sets `nasty_session` as an httpOnly,
// Secure, SameSite=Strict cookie on /api/login and /api/auth/oidc/callback.
// JS can't read the cookie, so XSS can't steal the session. Same-origin
// fetches and same-origin WS upgrades carry it automatically — there is no
// JS-visible token to thread through requests.

export async function login(username: string, password: string): Promise<void> {
	const res = await fetch('/api/login', {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify({ username, password }),
	});

	if (!res.ok) {
		const body = await res.json().catch(() => ({}));
		throw new Error(body.error || 'Login failed');
	}
	// Token is in the response body (for CLI tools that don't have a cookie
	// jar) but the WebUI doesn't read it — the Set-Cookie header is the
	// source of truth for the browser.
}

export async function logout(): Promise<void> {
	// Browsers can't delete an httpOnly cookie themselves, so logout has to
	// round-trip through the server: it revokes the session record and sends
	// Set-Cookie with Max-Age=0 to drop the cookie.
	await fetch('/api/logout', { method: 'POST' }).catch(() => {});
}
