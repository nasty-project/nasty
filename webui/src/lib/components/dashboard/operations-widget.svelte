<script lang="ts">
	import type { ActiveOperation } from '$lib/types';
	import type { DashboardDensity, DashboardWidgetPresentation } from '$lib/dashboard.svelte';
	import { Card, CardContent, CardHeader, CardTitle } from '$lib/components/ui/card';
	import { Activity, CheckCircle2, TriangleAlert } from '@lucide/svelte';

	let { operations, loaded, density, presentation }: { operations: ActiveOperation[]; loaded: boolean; density: DashboardDensity; presentation: DashboardWidgetPresentation } = $props();

	function kindLabel(kind: string): string {
		return ({ scrub: 'Scrub', evacuate: 'Evacuate', reconcile: 'Reconcile', copygc: 'Copy-GC', copy: 'File copy' } as Record<string, string>)[kind] ?? kind;
	}
</script>

{#if presentation === 'tiny'}
	<Card>
		<CardContent class="flex min-h-10 items-center gap-2 px-3 py-2 text-xs" aria-live="polite">
			{#if !loaded}
				<TriangleAlert class="h-4 w-4 shrink-0 text-muted-foreground" />
				<span class="font-medium text-muted-foreground">Operation status unavailable</span>
			{:else if operations.length === 0}
				<CheckCircle2 class="h-4 w-4 shrink-0 text-emerald-400" />
				<span class="font-medium">No active operations</span>
			{:else}
				<Activity class="h-4 w-4 shrink-0 text-amber-400" />
				<span class="min-w-0 truncate font-medium text-amber-300" title={operations[0].detail}>{operations.length} active - {kindLabel(operations[0].kind)} on {operations[0].fs}</span>
				<a href="/operations" class="ml-auto shrink-0 text-primary hover:underline">Open operations</a>
			{/if}
		</CardContent>
	</Card>
{:else}
	<Card>
		<CardHeader class={density === 'compact' ? 'px-4 py-2.5' : 'pb-2'}>
			<CardTitle class="flex items-center justify-between gap-3 text-xs uppercase tracking-wide text-muted-foreground">
				<span class="flex items-center gap-2">{#if !loaded}<TriangleAlert class="h-4 w-4 text-muted-foreground" />{:else if operations.length > 0}<Activity class="h-4 w-4 text-amber-400" />{:else}<CheckCircle2 class="h-4 w-4 text-emerald-400" />{/if} Active operations</span>
				<a href="/operations" class="normal-case tracking-normal text-primary hover:underline">Open operations</a>
			</CardTitle>
		</CardHeader>
		<CardContent class={density === 'compact' ? 'px-4 pb-3' : ''}>
			{#if !loaded}
				<p class="text-sm text-muted-foreground">Operation status is unavailable.</p>
			{:else if operations.length === 0}
				<p class="text-sm text-muted-foreground">No scrub, evacuation, or reconciliation work is active.</p>
			{:else}
				<div class="grid gap-2 md:grid-cols-2">
					{#each operations as operation}
						<div class="rounded-md border border-border px-3 py-2">
							<div class="flex items-center gap-2"><span class="text-sm font-semibold">{kindLabel(operation.kind)}</span><span class="font-mono text-xs text-muted-foreground">{operation.fs}</span>{#if operation.progress_percent != null}<span class="ml-auto text-xs tabular-nums text-amber-400">{operation.progress_percent.toFixed(0)}%</span>{/if}</div>
							<p class="mt-1 truncate text-xs text-muted-foreground" title={operation.detail}>{operation.detail}</p>
							{#if operation.progress_percent != null}<div class="mt-2 h-1.5 overflow-hidden rounded bg-muted"><div class="h-full bg-amber-500" style="width: {Math.max(0, Math.min(100, operation.progress_percent))}%"></div></div>{/if}
						</div>
					{/each}
				</div>
			{/if}
		</CardContent>
	</Card>
{/if}
