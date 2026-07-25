export interface PublicShareRoot {
	root: number;
	name: string;
	is_dir: boolean;
	size: number;
}

export interface PublicShareMeta {
	entries: PublicShareRoot[];
	password_required: boolean;
	unlocked: boolean;
	expires_at: number | null;
}

export interface PublicDirectoryEntry {
	name: string;
	is_dir: boolean;
	size: number;
}

export interface PublicDirectoryListing {
	root: number;
	path: string;
	entries: PublicDirectoryEntry[];
}

export interface ShareBreadcrumb {
	label: string;
	path: string;
}

export function normalizeShareDownloadLimit(value: number | undefined): number | null {
	if (value === undefined) return null;
	if (!Number.isSafeInteger(value) || value < 1) {
		throw new Error('Download limit must be a positive whole number');
	}
	return value;
}

export function joinSharePath(path: string, name: string): string {
	return path ? `${path}/${name}` : name;
}

export function parentSharePath(path: string): string {
	const separator = path.lastIndexOf('/');
	return separator < 0 ? '' : path.slice(0, separator);
}

export function shareBreadcrumbs(rootName: string, path: string): ShareBreadcrumb[] {
	const breadcrumbs: ShareBreadcrumb[] = [{ label: rootName, path: '' }];
	let current = '';
	for (const component of path.split('/').filter(Boolean)) {
		current = joinSharePath(current, component);
		breadcrumbs.push({ label: component, path: current });
	}
	return breadcrumbs;
}

function shareEndpoint(token: string, action = ''): string {
	const base = `/api/public/share/${encodeURIComponent(token)}`;
	return action ? `${base}/${action}` : base;
}

export function shareBrowseUrl(token: string, root: number, path: string): string {
	const params = new URLSearchParams({ root: String(root) });
	if (path) params.set('path', path);
	return `${shareEndpoint(token, 'browse')}?${params}`;
}

export function shareDownloadUrl(token: string, root: number, path: string): string {
	const params = new URLSearchParams({ root: String(root) });
	if (path) params.set('path', path);
	return `${shareEndpoint(token, 'download')}?${params}`;
}

export function shareZipUrl(token: string): string {
	return shareEndpoint(token, 'zip');
}
