export interface PortalBreadcrumb {
	label: string;
	path: string;
}

function validateSegment(segment: string): string {
	if (!segment || segment === '.' || segment === '..' || segment.includes('/') || segment.includes('\\')) {
		throw new Error('Invalid portal path segment');
	}
	if ([...segment].some((character) => character.charCodeAt(0) < 32)) {
		throw new Error('Invalid portal path segment');
	}
	return segment;
}

export function portalPathSegments(path: string): string[] {
	if (!path) return [];
	if (path.startsWith('/') || path.includes('\\')) throw new Error('Portal paths must be relative');
	return path.split('/').map(validateSegment);
}

export function joinPortalPath(path: string, name: string): string {
	return [...portalPathSegments(path), validateSegment(name)].join('/');
}

export function parentPortalPath(path: string): string {
	return portalPathSegments(path).slice(0, -1).join('/');
}

export function portalBreadcrumbs(path: string): PortalBreadcrumb[] {
	const segments = portalPathSegments(path);
	return segments.map((label, index) => ({
		label,
		path: segments.slice(0, index + 1).join('/'),
	}));
}

export function portalFilesUrl(
	action: 'browse' | 'content',
	share: string,
	path = '',
): string {
	const params = new URLSearchParams({ share, path });
	return `/api/user/files/${action}?${params.toString()}`;
}
