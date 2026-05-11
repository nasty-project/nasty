<script lang="ts">
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	interface Props {
		host: string;
		options: string;
	}
	let { host = $bindable(), options = $bindable() }: Props = $props();
</script>

<div class="mb-4">
	<Label>Allowed Network</Label>
	<Input bind:value={host} placeholder="192.168.1.0/24 or * for any" class="mt-1" />
</div>
<div class="mb-4">
	<Label>Export Options</Label>
	<Input bind:value={options} class="mt-1" />
</div>
