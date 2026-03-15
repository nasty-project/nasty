<script lang="ts">
	import { confirmState, confirmRespond } from '$lib/confirm.svelte';
	import {
		Dialog,
		DialogContent,
		DialogHeader,
		DialogTitle,
		DialogDescription,
		DialogFooter,
	} from '$lib/components/ui/dialog';
	import { Button } from '$lib/components/ui/button';
</script>

<Dialog bind:open={confirmState.open}>
	<DialogContent class="max-w-sm">
		<DialogHeader>
			<DialogTitle>{confirmState.title}</DialogTitle>
			{#if confirmState.message}
				<DialogDescription>{confirmState.message}</DialogDescription>
			{/if}
		</DialogHeader>
		<DialogFooter class="gap-2">
			<Button variant="outline" onclick={() => confirmRespond(false)}>Cancel</Button>
			<Button variant="destructive" onclick={() => confirmRespond(true)}>Confirm</Button>
		</DialogFooter>
	</DialogContent>
</Dialog>
