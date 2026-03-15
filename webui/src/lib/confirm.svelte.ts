/**
 * Imperative confirm dialog — drop-in replacement for window.confirm().
 * Mount <ConfirmDialog /> once in the root layout, then call:
 *   if (!await confirm('Title', 'Message')) return;
 */

interface ConfirmState {
	open: boolean;
	title: string;
	message: string;
	resolve: ((v: boolean) => void) | null;
}

export const confirmState = $state<ConfirmState>({
	open: false,
	title: '',
	message: '',
	resolve: null,
});

export function confirm(title: string, message: string): Promise<boolean> {
	return new Promise((resolve) => {
		confirmState.title = title;
		confirmState.message = message;
		confirmState.resolve = resolve;
		confirmState.open = true;
	});
}

export function confirmRespond(value: boolean) {
	confirmState.open = false;
	confirmState.resolve?.(value);
	confirmState.resolve = null;
}
