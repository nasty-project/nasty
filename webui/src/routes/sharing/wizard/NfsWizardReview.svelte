<script lang="ts">
	interface Props {
		host: string;
		options: string;
	}
	let { host, options }: Props = $props();
</script>

<span class="text-muted-foreground">Allowed</span>
<span>{host || '*'}</span>
<span class="text-muted-foreground">Options</span>
<span class="text-xs">{options}</span>
