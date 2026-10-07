export interface ListenerPorts { https_port: number; http_port: number | null }
export interface ListenerState {
	confirmed: ListenerPorts;
	pending: { txn_id: string; ports: ListenerPorts; deadline: number } | null;
	last_error: string | null;
}
export function validListenerPorts(https: number, http: number | null): boolean {
	const valid = (p: number) => Number.isInteger(p) && p > 0 && p <= 65535 && ![2019, 2137].includes(p);
	return valid(https) && (http === null || (valid(http) && http !== https));
}
export function listenerUrl(current: string, port: number): string {
	const url = new URL(current);
	url.protocol = 'https:';
	url.port = port === 443 ? '' : String(port);
	url.pathname = '/settings';
	url.search = '';
	url.hash = '';
	return url.href;
}
export function onListener(current: string, port: number): boolean {
	const url = new URL(current);
	return url.protocol === 'https:' && Number(url.port || 443) === port;
}
