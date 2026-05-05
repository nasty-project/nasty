<script lang="ts">
	let licenseText = $state('');
	let thirdPartyText = $state('');

	async function load(path: string): Promise<string> {
		const res = await fetch(path);
		return res.ok ? await res.text() : 'Failed to load.';
	}

	$effect(() => {
		load('/LICENSE.txt').then(t => licenseText = t);
		load('/THIRD-PARTY-LICENSES.md').then(t => thirdPartyText = t);
	});
</script>

<div class="mx-auto max-w-4xl space-y-8 p-6">
	<h1 class="text-xl font-semibold">Licenses</h1>

	<section>
		<h2 class="mb-3 text-base font-semibold">NASty License (GPL-3.0)</h2>
		<pre class="max-h-96 overflow-auto rounded-lg border border-border bg-muted/30 p-4 text-xs leading-relaxed">{licenseText}</pre>
	</section>

	<section>
		<h2 class="mb-3 text-base font-semibold">Third-Party Licenses</h2>
		{#each thirdPartyText.split('\n## ') as section, i}
			{#if i === 0}
				<p class="mb-4 whitespace-pre-line text-sm text-muted-foreground">{section.replace(/^# .*\n+/, '')}</p>
			{:else}
				{@const lines = section.split('\n')}
				{@const title = lines[0]}
				{@const rows = lines.filter(l => l.startsWith('| ') && !l.startsWith('| -') && !l.startsWith('|--') && !l.includes('Crate') && !l.includes('Package'))}
				<div class="mb-6">
					<h3 class="mb-2 text-sm font-semibold">{title}</h3>
					<div class="rounded-lg border border-border overflow-hidden">
						<table class="w-full text-sm">
							<thead>
								<tr class="border-b border-border bg-muted/50">
									<th class="px-3 py-2 text-left text-xs font-medium text-muted-foreground">Package</th>
									<th class="px-3 py-2 text-left text-xs font-medium text-muted-foreground">License</th>
								</tr>
							</thead>
							<tbody>
								{#each rows as row}
									{@const cells = row.split('|').filter(c => c.trim()).map(c => c.trim())}
									{#if cells.length >= 2}
										<tr class="border-b border-border/50 last:border-0">
											<td class="px-3 py-1.5 font-mono text-xs">{cells[0]}</td>
											<td class="px-3 py-1.5 text-xs text-muted-foreground">{cells[1]}</td>
										</tr>
									{/if}
								{/each}
							</tbody>
						</table>
					</div>
				</div>
			{/if}
		{/each}
	</section>
</div>
