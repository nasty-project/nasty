<script lang="ts">
	interface Props {
		name: string;
		fallbackName: string;
		addr: string;
		port: string;
	}
	let { name, fallbackName, addr, port }: Props = $props();
</script>

<span class="text-muted-foreground">Subsystem</span>
<span>{name || fallbackName}</span>
<span class="text-muted-foreground">Listen</span>
<span>{addr}:{port}</span>
