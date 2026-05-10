<script lang="ts">
	import { unlockFsState, unlockFsRespond } from '$lib/unlock-fs.svelte';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import {
		Dialog,
		DialogContent,
		DialogHeader,
		DialogTitle,
		DialogDescription,
		DialogFooter,
	} from '$lib/components/ui/dialog';
	import { Button } from '$lib/components/ui/button';

	let passphrase = $state('');
	let submitting = $state(false);

	async function submit() {
		if (!passphrase || submitting) return;
		const name = unlockFsState.fsName;
		submitting = true;
		const result = await withToast(
			() => getClient().call('fs.unlock', { name, passphrase }),
			`Filesystem "${name}" unlocked`,
		);
		submitting = false;
		passphrase = '';
		// `withToast` resolves to undefined on RPC failure (the
		// toast renders the error). Either way the dialog closes —
		// the caller decides what to do next based on the boolean.
		unlockFsRespond(result !== undefined);
	}

	function cancel() {
		passphrase = '';
		unlockFsRespond(false);
	}

	// Wipe the passphrase whenever the dialog re-opens for a different
	// filesystem so a typed-but-not-submitted secret doesn't leak
	// across opens.
	$effect(() => {
		if (unlockFsState.open) {
			passphrase = '';
			submitting = false;
		}
	});
</script>

<Dialog
	open={unlockFsState.open}
	onOpenChange={(v) => {
		if (!v) cancel();
	}}
>
	<DialogContent class="max-w-sm">
		<DialogHeader>
			<DialogTitle>Unlock "{unlockFsState.fsName}"</DialogTitle>
			<DialogDescription>
				Enter the passphrase to unlock this encrypted filesystem.
			</DialogDescription>
		</DialogHeader>
		<input
			type="password"
			bind:value={passphrase}
			class="h-9 w-full rounded-md border border-input bg-transparent px-3 text-sm"
			placeholder="Passphrase"
			onkeydown={(e) => {
				if (e.key === 'Enter') submit();
			}}
			disabled={submitting}
		/>
		<DialogFooter class="gap-2">
			<Button variant="outline" onclick={cancel} disabled={submitting}>Cancel</Button>
			<Button onclick={submit} disabled={!passphrase || submitting}>
				{submitting ? 'Unlocking…' : 'Unlock'}
			</Button>
		</DialogFooter>
	</DialogContent>
</Dialog>
