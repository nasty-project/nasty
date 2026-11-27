<script lang="ts">
	import IoChart from "$lib/components/io-chart.svelte";
	import {
	    Card,
	    CardContent,
	    CardHeader,
	    CardTitle,
	} from "$lib/components/ui/card";
	import type { DashboardDensity } from "$lib/dashboard.svelte";
	import { formatBytes } from "$lib/format";
	import type { DiskIoStats } from "$lib/types";

	type Rate = { readRate: number; writeRate: number };
	type Sample = { time: Date; in: number; out: number };
	let {
		devices,
		rates,
		samples,
		density,
	}: {
		devices: DiskIoStats[];
		rates: Map<string, Rate>;
		samples: Map<string, Sample[]>;
		density: DashboardDensity;
	} = $props();

	const readColor = "var(--chart-2)";
	const writeColor = "var(--chart-4)";
</script>

<Card class="h-full">
	<CardHeader class={density === "compact" ? "px-4 py-2.5" : "pb-2"}
		><CardTitle
			class="text-xs uppercase tracking-wide text-muted-foreground"
			>Disk I/O</CardTitle
		></CardHeader
	>
	<CardContent class={density === "compact" ? "px-4 pb-3" : ""}>
		{#if devices.length === 0}
			<p class="text-sm text-muted-foreground">
				No disk activity reported.
			</p>
		{:else}
			<div class="divide-y divide-border">
				{#each devices as device}
					{@const rate = rates.get(device.name)}
					<div
						class={density === "compact"
							? "py-2 first:pt-0 last:pb-0"
							: "py-2.5 first:pt-0 last:pb-0"}
					>
						<div class="mb-1.5 flex items-center gap-2">
							<span class="text-sm font-semibold">
								{device.name}
							</span>
							{#if device.io_in_progress > 0}
								<span
									class="rounded bg-amber-500/15 px-1.5 py-0.5 text-[0.65rem] font-medium text-amber-500"
								>
									{device.io_in_progress} active
								</span>
							{/if}
							<span
								class="ml-auto text-xs tabular-nums text-muted-foreground"
							>
								{formatBytes(
									device.read_bytes + device.write_bytes,
								)}
							</span>
						</div>
						<div class="mb-2 flex gap-5 text-xs">
							<span>
								<b class="text-muted-foreground">R</b>
								<span style="color:{readColor};">
									{rate
										? `${formatBytes(rate.readRate)}/s`
										: "-"}
								</span>
							</span>
							<span>
								<b class="text-muted-foreground">W</b>
								<span style="color:{writeColor};">
									{rate
										? `${formatBytes(rate.writeRate)}/s`
										: "-"}
								</span>
							</span>
						</div>
						<IoChart
							samples={samples.get(device.name) ?? []}
							inLabel="Read"
							outLabel="Write"
							inColor={readColor}
							outColor={writeColor}
							ariaLabel={`${device.name} disk throughput history`}
						/>
					</div>
				{/each}
			</div>
		{/if}
	</CardContent>
</Card>
