<script lang="ts">
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	interface Props {
		name: string;
		addr: string;
		port: string;
	}
	let { name = $bindable(), addr = $bindable(), port = $bindable() }: Props = $props();
</script>

<div class="mb-4">
	<Label>Subsystem Name</Label>
	<Input bind:value={name} placeholder="storage-vol" class="mt-1" />
</div>
<div class="grid grid-cols-2 gap-4 mb-4">
	<div>
		<Label>Listen Address</Label>
		<Input bind:value={addr} placeholder="0.0.0.0" class="mt-1" />
	</div>
	<div>
		<Label>Port</Label>
		<Input bind:value={port} placeholder="4420" class="mt-1" />
	</div>
</div>
