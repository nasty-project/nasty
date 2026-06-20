<script lang="ts">
	// Guest Shares moved into the Sharing page as a tab (alongside SMB/NFS/
	// iSCSI/NVMe-oF). This route is kept as a redirect so existing links and
	// bookmarks to /shares still land on the right place.
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';

	onMount(() => {
		goto('/sharing#guest', { replaceState: true });
	});
</script>

<p class="p-6 text-sm text-muted-foreground">Redirecting to Sharing → Guest Shares…</p>
