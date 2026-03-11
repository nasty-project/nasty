<script lang="ts">
	import { getToasts, dismiss, type Toast } from '$lib/toast.svelte';

	const toasts = $derived(getToasts());
</script>

{#if toasts.length > 0}
	<div class="toast-container">
		{#each toasts as toast (toast.id)}
			<div class="toast toast-{toast.type}" role="alert">
				<span class="toast-icon">
					{#if toast.type === 'success'}&#10003;{:else if toast.type === 'error'}&#10007;{:else}&#9432;{/if}
				</span>
				<span class="toast-msg">{toast.message}</span>
				<button class="toast-close" onclick={() => dismiss(toast.id)} aria-label="Dismiss">&times;</button>
			</div>
		{/each}
	</div>
{/if}

<style>
	.toast-container {
		position: fixed;
		top: 1rem;
		right: 1rem;
		z-index: 1000;
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		max-width: 420px;
	}
	.toast {
		display: flex;
		align-items: start;
		gap: 0.6rem;
		padding: 0.75rem 1rem;
		border-radius: 6px;
		font-size: 0.875rem;
		box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4);
		animation: slide-in 0.2s ease-out;
	}
	.toast-success {
		background: #064e3b;
		border: 1px solid #065f46;
		color: #a7f3d0;
	}
	.toast-error {
		background: #450a0a;
		border: 1px solid #7f1d1d;
		color: #fca5a5;
	}
	.toast-info {
		background: #1e2130;
		border: 1px solid #2d3348;
		color: #e0e0e0;
	}
	.toast-icon {
		font-size: 1rem;
		flex-shrink: 0;
		line-height: 1.3;
	}
	.toast-msg {
		flex: 1;
		line-height: 1.4;
		word-break: break-word;
	}
	.toast-close {
		background: none;
		border: none;
		color: inherit;
		font-size: 1.1rem;
		cursor: pointer;
		padding: 0;
		opacity: 0.6;
		flex-shrink: 0;
		line-height: 1;
	}
	.toast-close:hover {
		opacity: 1;
		background: none;
	}
	@keyframes slide-in {
		from { transform: translateX(100%); opacity: 0; }
		to { transform: translateX(0); opacity: 1; }
	}
</style>
