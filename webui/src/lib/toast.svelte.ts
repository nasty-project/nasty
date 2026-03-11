/** Toast notification store */

type ToastType = 'success' | 'error' | 'info';

export interface Toast {
	id: number;
	type: ToastType;
	message: string;
}

let nextId = 0;
let toasts = $state<Toast[]>([]);

export function getToasts(): Toast[] {
	return toasts;
}

function add(type: ToastType, message: string, durationMs = 5000) {
	const id = nextId++;
	toasts.push({ id, type, message });

	if (durationMs > 0) {
		setTimeout(() => dismiss(id), durationMs);
	}
}

export function dismiss(id: number) {
	toasts = toasts.filter((t) => t.id !== id);
}

export function success(message: string) {
	add('success', message);
}

export function error(message: string) {
	add('error', message, 8000);
}

export function info(message: string) {
	add('info', message);
}

/** Wrap an async RPC call with automatic error toast */
export async function withToast<T>(
	fn: () => Promise<T>,
	successMsg?: string
): Promise<T | undefined> {
	try {
		const result = await fn();
		if (successMsg) success(successMsg);
		return result;
	} catch (e: unknown) {
		const msg = e instanceof Error ? e.message : typeof e === 'object' && e !== null && 'message' in e ? String((e as { message: unknown }).message) : String(e);
		error(msg);
		return undefined;
	}
}
