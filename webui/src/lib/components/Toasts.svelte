<script lang="ts">
	import { getToasts, dismiss, type Toast } from '$lib/toast.svelte';

	const toasts = $derived(getToasts());
</script>

{#if toasts.length > 0}
	<div class="fixed bottom-4 right-4 z-[1000] flex max-w-[420px] flex-col-reverse gap-2">
		{#each toasts as toast (toast.id)}
			<div
				class="flex animate-in slide-in-from-bottom-2 items-start gap-2.5 rounded-lg border px-4 py-3 text-sm shadow-lg {
					toast.type === 'success' ? 'border-green-900 bg-green-950 text-green-200' :
					toast.type === 'error' ? 'border-red-900 bg-red-950 text-red-200' :
					'border-border bg-card text-foreground'
				}"
				role="alert"
			>
				<span class="shrink-0 text-base leading-5">
					{#if toast.type === 'success'}&#10003;{:else if toast.type === 'error'}&#10007;{:else}&#9432;{/if}
				</span>
				<span class="flex-1 break-words leading-snug">{toast.message}</span>
				<button
					class="shrink-0 bg-transparent p-0 text-lg leading-none opacity-60 hover:bg-transparent hover:opacity-100"
					onclick={() => dismiss(toast.id)}
					aria-label="Dismiss"
				>&times;</button>
			</div>
		{/each}
	</div>
{/if}
