<script lang="ts">
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	interface Props {
		name: string;
	}
	let { name = $bindable() }: Props = $props();
</script>

<div class="mb-4">
	<Label>Target Name</Label>
	<Input bind:value={name} placeholder="dbserver" class="mt-1" />
	<p class="mt-1 text-xs text-muted-foreground">IQN: iqn.2137-01.com.nasty:{name || '...'}</p>
</div>
