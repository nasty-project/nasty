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
	import type { NetIfStats } from "$lib/types";

	type Rate = { rxRate: number; txRate: number };
	type Sample = { time: Date; in: number; out: number };
	let {
		interfaces,
		rates,
		samples,
		density,
	}: {
		interfaces: NetIfStats[];
		rates: Map<string, Rate>;
		samples: Map<string, Sample[]>;
		density: DashboardDensity;
	} = $props();

	function ipv4(addresses: string[]): string[] {
		return addresses.filter((address) => !address.includes(":"));
	}

	const rxColor = "var(--chart-2)";
	const txColor = "var(--chart-1)";
</script>

<Card class="h-full">
	<CardHeader class={density === "compact" ? "px-4 py-2.5" : "pb-2"}
		><CardTitle
			class="text-xs uppercase tracking-wide text-muted-foreground"
			>Network</CardTitle
		></CardHeader
	>
	<CardContent class={density === "compact" ? "px-4 pb-3" : ""}>
		{#if interfaces.length === 0}
			<p class="text-sm text-muted-foreground">
				No network interfaces reported.
			</p>
		{:else}
			<div class="divide-y divide-border">
				{#each interfaces as iface}
					{@const rate = rates.get(iface.name)}
					{@const addresses = ipv4(iface.addresses)}
					<div
						class={density === "compact"
							? "py-2 first:pt-0 last:pb-0"
							: "py-2.5 first:pt-0 last:pb-0"}
					>
						<div class="mb-1.5 flex flex-wrap items-center gap-2">
							<span class="text-sm font-semibold"
								>{iface.name}</span
							><span
								class="h-2 w-2 rounded-full {iface.up
									? 'bg-emerald-400'
									: 'bg-red-400'}"
							></span><span class="sr-only"
								>{iface.up ? "up" : "down"}</span
							>{#if iface.speed_mbps}<span
									class="text-xs text-muted-foreground"
									>{iface.speed_mbps >= 1000
										? `${iface.speed_mbps / 1000}G`
										: `${iface.speed_mbps}M`}</span
								>{/if}{#if addresses.length > 0}<span
									class="ml-auto font-mono text-xs"
									>{addresses.join(", ")}</span
								>{/if}
						</div>
						<div class="mb-2 flex gap-5 text-xs">
							<span
								><b class="text-muted-foreground">RX</b>
								<span style="color:{rxColor};"
									>{rate
										? `${formatBytes(rate.rxRate)}/s`
										: "-"}</span
								></span
							><span
								><b class="text-muted-foreground">TX</b>
								<span style="color:{txColor};"
									>{rate
										? `${formatBytes(rate.txRate)}/s`
										: "-"}</span
								></span
							><span
								class="ml-auto tabular-nums text-muted-foreground"
								>{formatBytes(iface.rx_bytes + iface.tx_bytes)} total</span
							>
						</div>
						<IoChart
							samples={samples.get(iface.name) ?? []}
							inLabel="RX"
							outLabel="TX"
							inColor={rxColor}
							outColor={txColor}
							ariaLabel={`${iface.name} network throughput history`}
						/>
					</div>
				{/each}
			</div>
		{/if}
	</CardContent>
</Card>
