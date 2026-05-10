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
	<DialogContent class="max-w-lg">
		<DialogHeader>
			<DialogTitle>{confirmState.title}</DialogTitle>
			{#if confirmState.message}
				<!-- whitespace-pre-line preserves embedded `\n` as line breaks
				     while collapsing other whitespace runs. Lets callers pass
				     multi-line messages (e.g. the FS-dependents impact preview)
				     without having to switch to a richer dialog component. -->
				<DialogDescription class="whitespace-pre-line"
					>{confirmState.message}</DialogDescription
				>
			{/if}
		</DialogHeader>
		<DialogFooter class="gap-2">
			<Button variant="outline" onclick={() => confirmRespond(false)}>{confirmState.cancelLabel}</Button>
			<Button variant="destructive" onclick={() => confirmRespond(true)}>{confirmState.confirmLabel}</Button>
		</DialogFooter>
	</DialogContent>
</Dialog>
