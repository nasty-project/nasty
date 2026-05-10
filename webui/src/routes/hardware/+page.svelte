<script lang="ts">
	import { onMount } from 'svelte';
	import { getClient } from '$lib/client';
	import { Button } from '$lib/components/ui/button';
	import { Badge } from '$lib/components/ui/badge';
	import { Card, CardContent } from '$lib/components/ui/card';
	import type { IommuGroup, PciDevice } from '$lib/types';
	import { ChevronDown, ChevronRight, RefreshCw } from '@lucide/svelte';

	let groups: IommuGroup[] = $state([]);
	let loading = $state(true);
	let refreshing = $state(false);
	let expanded = $state(new Set<number>());
	let filter = $state('');

	const client = getClient();

	async function load() {
		try {
			groups = await client.call<IommuGroup[]>('system.hardware.iommu');
		} catch {
			groups = [];
		}
		loading = false;
	}

	async function refresh() {
		refreshing = true;
		await load();
		refreshing = false;
	}

	function toggle(id: number) {
		if (expanded.has(id)) expanded.delete(id);
		else expanded.add(id);
		expanded = new Set(expanded);
	}

	function expandAll() {
		expanded = new Set(filteredGroups.map((g) => g.id));
	}

	function collapseAll() {
		expanded = new Set();
	}

	/** Single-line description for a device. Falls back through human
	 * names → numeric IDs so the user always sees something useful even
	 * on bleeding-edge hardware where pci.ids has no entry. */
	function describe(d: PciDevice): string {
		const vendor = d.vendor_name ?? d.vendor_id;
		const device = d.device_name ?? d.device_id;
		return `${vendor} ${device}`;
	}

	/** Group is "active for passthrough" if any device in it is bound to
	 * vfio-pci. We highlight these so users can see at a glance which
	 * groups are currently claimed for VMs. */
	function isPassthroughGroup(g: IommuGroup): boolean {
		return g.devices.some((d) => d.driver === 'vfio-pci');
	}

	function driverBadgeVariant(driver: string | null): 'default' | 'secondary' | 'outline' {
		if (!driver) return 'outline';
		if (driver === 'vfio-pci') return 'default';
		return 'secondary';
	}

	let filteredGroups = $derived.by(() => {
		const q = filter.trim().toLowerCase();
		if (!q) return groups;
		return groups.filter((g) =>
			g.devices.some((d) => {
				const haystack = [
					d.bdf,
					d.vendor_id,
					d.device_id,
					d.vendor_name,
					d.device_name,
					d.class_name,
					d.driver,
				]
					.filter(Boolean)
					.join(' ')
					.toLowerCase();
				return haystack.includes(q);
			}),
		);
	});

	onMount(load);
</script>

<div class="mb-4 flex items-center justify-between">
	<div>
		<h1 class="text-2xl font-semibold">Hardware</h1>
		<p class="text-sm text-muted-foreground">
			PCI devices grouped by IOMMU group. Useful for planning VM passthrough — devices in the same
			group must be assigned together.
		</p>
	</div>
	<Button size="sm" variant="secondary" onclick={refresh} disabled={refreshing}>
		<RefreshCw class={refreshing ? 'animate-spin' : ''} size={14} />
		Refresh
	</Button>
</div>

{#if loading}
	<Card>
		<CardContent class="py-12 text-center text-sm text-muted-foreground">Loading…</CardContent>
	</Card>
{:else if groups.length === 0}
	<Card>
		<CardContent class="py-8 text-center">
			<p class="mb-1 font-medium">No IOMMU groups found</p>
			<p class="text-sm text-muted-foreground">
				IOMMU is likely off in BIOS. Enable VT-d (Intel) or AMD-Vi (AMD) and reboot. NASty's kernel
				params already pass <code class="font-mono">intel_iommu=on amd_iommu=on iommu=pt</code>.
			</p>
		</CardContent>
	</Card>
{:else}
	<div class="mb-3 flex items-center gap-3">
		<input
			bind:value={filter}
			placeholder="Filter by BDF, vendor, device, or driver…"
			class="h-9 flex-1 max-w-md rounded-md border border-input bg-background px-3 text-sm focus:outline-none focus:ring-2 focus:ring-ring"
		/>
		<Button size="xs" variant="ghost" onclick={expandAll}>Expand all</Button>
		<Button size="xs" variant="ghost" onclick={collapseAll}>Collapse all</Button>
		<span class="ml-auto text-xs text-muted-foreground">
			{filteredGroups.length} of {groups.length} groups
		</span>
	</div>

	<div class="space-y-2">
		{#each filteredGroups as group (group.id)}
			{@const passthrough = isPassthroughGroup(group)}
			<Card>
				<CardContent class="p-0">
					<button
						onclick={() => toggle(group.id)}
						class="flex w-full items-center gap-3 px-4 py-3 text-left hover:bg-accent/50"
					>
						{#if expanded.has(group.id)}
							<ChevronDown size={16} class="text-muted-foreground" />
						{:else}
							<ChevronRight size={16} class="text-muted-foreground" />
						{/if}
						<span class="font-mono text-sm">Group {group.id}</span>
						<span class="text-xs text-muted-foreground"
							>{group.devices.length} device{group.devices.length === 1 ? '' : 's'}</span
						>
						{#if passthrough}
							<Badge variant="default" class="ml-1 text-[0.6rem]">Passthrough</Badge>
						{/if}
						<span class="ml-auto truncate text-xs text-muted-foreground">
							{group.devices
								.map((d) => d.device_name ?? d.device_id)
								.slice(0, 2)
								.join(' · ')}{group.devices.length > 2 ? ' · …' : ''}
						</span>
					</button>

					{#if expanded.has(group.id)}
						<div class="border-t border-border bg-muted/20 px-4 py-3">
							<table class="w-full text-sm">
								<thead>
									<tr class="text-left text-[0.7rem] uppercase tracking-wide text-muted-foreground">
										<th class="pb-2 font-medium">BDF</th>
										<th class="pb-2 font-medium">Class</th>
										<th class="pb-2 font-medium">Device</th>
										<th class="pb-2 font-medium">IDs</th>
										<th class="pb-2 font-medium">Driver</th>
									</tr>
								</thead>
								<tbody>
									{#each group.devices as device (device.bdf)}
										<tr class="border-t border-border/40">
											<td class="py-2 pr-3 font-mono text-xs">{device.bdf}</td>
											<td class="py-2 pr-3 text-xs text-muted-foreground"
												>{device.class_name ?? device.class_id}</td
											>
											<td class="py-2 pr-3">{describe(device)}</td>
											<td class="py-2 pr-3 font-mono text-xs text-muted-foreground"
												>{device.vendor_id}:{device.device_id}</td
											>
											<td class="py-2">
												<Badge variant={driverBadgeVariant(device.driver)} class="text-[0.65rem]">
													{device.driver ?? 'unbound'}
												</Badge>
											</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{/if}
				</CardContent>
			</Card>
		{/each}
	</div>
{/if}
