<script lang="ts">
	import { diskUsagePresentation, type VmDiskCandidate } from '$lib/vm-disk-safety';
	import { Badge } from '$lib/components/ui/badge';
	let { candidates }: { candidates: VmDiskCandidate[] } = $props();
</script>

{#if candidates.length > 0}
	<details class="mt-3 rounded-md border border-border">
		<summary class="cursor-pointer px-3 py-2 text-xs text-muted-foreground hover:text-foreground">
			Unavailable disks <span class="ml-1 rounded bg-secondary px-1.5 py-0.5">{candidates.length}</span>
		</summary>
		<div class="border-t border-border px-3 pb-2">
			<p class="py-2 text-xs text-muted-foreground">These disks are reserved by another workload and cannot be attached. Expand a disk to see its volume and usage details.</p>
			<div class="divide-y divide-border">
				{#each candidates as candidate (`${candidate.subvolume.filesystem}/${candidate.subvolume.name}`)}
					{@const presentation = diskUsagePresentation(candidate)}
					<details class="py-2">
						<summary class="cursor-pointer text-xs">
							<span class="ml-1 break-all font-medium">{presentation.name}</span>
							<span class="ml-2 inline-flex flex-wrap gap-1 align-middle">
								{#each presentation.categories as category}<Badge variant="secondary" class="text-[0.6rem]">{category}</Badge>{/each}
							</span>
						</summary>
						<div class="mt-2 space-y-2 pl-4 text-xs text-muted-foreground">
							<div><span class="font-medium text-foreground">Volume</span><p class="break-all font-mono">{candidate.subvolume.filesystem}/{candidate.subvolume.name}</p></div>
							{#if candidate.subvolume.block_device}<div><span class="font-medium text-foreground">Device</span><p class="font-mono">{candidate.subvolume.block_device}</p></div>{/if}
							<div><span class="font-medium text-foreground">Used by</span><ul class="mt-1 list-disc space-y-1 pl-4">{#each candidate.consumers as consumer}<li class="break-words [overflow-wrap:anywhere]">{consumer}</li>{/each}</ul></div>
						</div>
					</details>
				{/each}
			</div>
		</div>
	</details>
{/if}
