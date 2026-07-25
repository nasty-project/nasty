import type { UserRole } from './types';

export const PORTAL_PATH = '/portal';

export function isStandardUser(role: string | null | undefined): role is 'user' {
	return role === 'user';
}

export function isManagementRole(role: string | null | undefined): role is Exclude<UserRole, 'user'> {
	return role === 'admin' || role === 'operator' || role === 'readonly';
}

export function canAccessAuthenticatedRoute(role: string, path: string): boolean {
	const portalRoute = path === PORTAL_PATH || path.startsWith(`${PORTAL_PATH}/`);
	return isStandardUser(role) ? portalRoute : !portalRoute;
}

export function redirectForRole(role: string, path: string): string | null {
	if (canAccessAuthenticatedRoute(role, path)) return null;
	return isStandardUser(role) ? PORTAL_PATH : '/';
}
