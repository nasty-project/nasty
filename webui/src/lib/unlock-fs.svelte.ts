/**
 * Imperative unlock-encrypted-FS dialog. Mount <UnlockFsDialog /> once
 * in the root layout, then call from anywhere:
 *
 *   if (await unlockFs('tank')) { /* refresh local state *\/ }
 *
 * Resolves to true when the unlock RPC succeeds (FS keyring now has
 * the key), false when the user cancels or the RPC fails. Toast
 * handling is built in — callers don't need their own withToast.
 *
 * Used by:
 *  - the Filesystems page (replaces its inline modal),
 *  - the Apps and VMs pages, where a locked-FS badge offers
 *    inline unlock without forcing the user to navigate away.
 */

interface UnlockFsState {
	open: boolean;
	fsName: string;
	resolve: ((v: boolean) => void) | null;
}

export const unlockFsState = $state<UnlockFsState>({
	open: false,
	fsName: '',
	resolve: null,
});

export function unlockFs(fsName: string): Promise<boolean> {
	return new Promise((resolve) => {
		unlockFsState.fsName = fsName;
		unlockFsState.resolve = resolve;
		unlockFsState.open = true;
	});
}

export function unlockFsRespond(value: boolean) {
	unlockFsState.open = false;
	unlockFsState.resolve?.(value);
	unlockFsState.resolve = null;
	unlockFsState.fsName = '';
}
