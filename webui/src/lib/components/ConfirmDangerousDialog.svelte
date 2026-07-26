<script lang="ts">
	import { confirmDangerousState, confirmDangerousRespond } from '$lib/confirm-dangerous.svelte';
	import {
		Dialog,
		DialogContent,
		DialogHeader,
		DialogTitle,
		DialogDescription,
		DialogFooter,
	} from '$lib/components/ui/dialog';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';

	let inputValue = $state('');
	let matches = $derived(inputValue === confirmDangerousState.expectedValue);

	$effect(() => {
		void confirmDangerousState.open;
		inputValue = '';
	});
</script>

<Dialog
	open={confirmDangerousState.open}
	onOpenChange={(open) => {
		if (!open && confirmDangerousState.open) confirmDangerousRespond(false);
	}}
>
	<DialogContent class="max-w-lg">
		<DialogHeader>
			<DialogTitle>{confirmDangerousState.title}</DialogTitle>
			{#if confirmDangerousState.message}
				<DialogDescription>{confirmDangerousState.message}</DialogDescription>
			{/if}
		</DialogHeader>
		<Input
			bind:value={inputValue}
			placeholder={confirmDangerousState.expectedValue}
			onkeydown={(e) => { if (e.key === 'Enter' && matches) confirmDangerousRespond(true); }}
		/>
		<DialogFooter class="gap-2">
			<Button variant="outline" onclick={() => confirmDangerousRespond(false)}>Cancel</Button>
			<Button variant="destructive" disabled={!matches} onclick={() => confirmDangerousRespond(true)}>Destroy</Button>
		</DialogFooter>
	</DialogContent>
</Dialog>
