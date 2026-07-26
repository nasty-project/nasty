/**
 * Imperative confirm dialog — drop-in replacement for window.confirm().
 * Mount <ConfirmDialog /> once in the root layout, then call:
 *   if (!await confirm('Title', 'Message')) return;
 */

import { registerSessionReset } from './client';

interface ConfirmState {
	open: boolean;
	title: string;
	message: string;
	confirmLabel: string;
	cancelLabel: string;
	resolve: ((v: boolean) => void) | null;
}

export const confirmState = $state<ConfirmState>({
	open: false,
	title: '',
	message: '',
	confirmLabel: 'Confirm',
	cancelLabel: 'Cancel',
	resolve: null,
});

interface ConfirmOptions {
	confirmLabel?: string;
	cancelLabel?: string;
}

export function confirm(title: string, message?: string, options?: ConfirmOptions): Promise<boolean> {
	return new Promise((resolve) => {
		confirmState.resolve?.(false);
		confirmState.title = title;
		confirmState.message = message ?? '';
		confirmState.confirmLabel = options?.confirmLabel ?? 'Confirm';
		confirmState.cancelLabel = options?.cancelLabel ?? 'Cancel';
		confirmState.resolve = resolve;
		confirmState.open = true;
	});
}

export function confirmRespond(value: boolean) {
	confirmState.open = false;
	confirmState.resolve?.(value);
	confirmState.resolve = null;
	confirmState.title = '';
	confirmState.message = '';
	confirmState.confirmLabel = 'Confirm';
	confirmState.cancelLabel = 'Cancel';
}

registerSessionReset(() => confirmRespond(false));
