<script lang="ts">
	interface Props {
		name: string;
		fallbackName: string;
		guestOk: boolean;
		readOnly: boolean;
		validUsers: string[];
		timeMachine?: boolean;
		maxSizeGib?: number | null;
	}
	let { name, fallbackName, guestOk, readOnly, validUsers, timeMachine = false, maxSizeGib = null }: Props = $props();
</script>

<span class="text-muted-foreground">Share Name</span>
<span>{name || fallbackName}</span>
{#if timeMachine}
	<span class="text-muted-foreground">Time Machine</span>
	<span>Enabled{maxSizeGib ? ` — max ${maxSizeGib} GiB` : ''}</span>
{/if}
{#if guestOk}
	<span class="text-muted-foreground">Guests</span>
	<span>Allowed</span>
{/if}
{#if readOnly}
	<span class="text-muted-foreground">Access</span>
	<span>Read-only</span>
{/if}
<span class="text-muted-foreground">Allowed Users</span>
<span>{validUsers.length > 0 ? validUsers.join(', ') : 'All authenticated users'}</span>
