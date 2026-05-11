<script lang="ts">
	interface Props {
		name: string;
		fallbackName: string;
	}
	let { name, fallbackName }: Props = $props();
</script>

<span class="text-muted-foreground">Target</span>
<span class="font-mono text-xs">iqn.2137-01.com.nasty:{name || fallbackName}</span>
