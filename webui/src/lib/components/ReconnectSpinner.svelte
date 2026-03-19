<script lang="ts">
	// Each ring starts spinning from a random angle so they don't all begin aligned.
	function makeRing(size: number, color: string, head: string, glow: string, dur: number, cw: boolean, dot: number, trail: number) {
		const startDeg = Math.floor(Math.random() * 360);
		return {
			size, color, head, glow, dur, dir: cw ? '' : 'reverse', dot, trail,
			border: color + '18', startDeg,
			gradient: cw
				? `transparent 40%, ${color}40 70%, ${color} 90%, ${head} 100%`
				: `${head} 0%, ${color} 10%, ${color}40 30%, transparent 60%`,
		};
	}
	const rings = [
		makeRing(256, '#3b82f6', '#93c5fd', '#3b82f680', 2.2, false, 14, 3),
		makeRing(224, '#eab308', '#fde047', '#eab30880', 2.5, true,  12, 3),
		makeRing(192, '#f97316', '#fdba74', '#f9731680', 3,   false, 10, 2.5),
		makeRing(160, '#ef4444', '#fca5a5', '#ef444480', 3.5, true,  10, 2.5),
		makeRing(128, '#22c55e', '#86efac', '#22c55e80', 4,   false, 8,  2),
	];
</script>

<div class="relative flex items-center justify-center" style="width: 270px; height: 270px;">
	{#each rings as ring}
		{@const r = ring.size / 2}
		{@const dotR = ring.dot / 2}
		<!-- Head angle: conic-gradient "from Xdeg" means 100% lands at Xdeg (0deg = top, clockwise) -->
		{@const headAngle = ring.startDeg}
		{@const headRad = (headAngle - 90) * Math.PI / 180}
		{@const dotX = r + r * Math.cos(headRad) - dotR}
		{@const dotY = r + r * Math.sin(headRad) - dotR}

		<!-- Comet trail + head as one spinning unit -->
		<div
			class="absolute"
			style="
				width: {ring.size}px; height: {ring.size}px;
				animation: spin {ring.dur}s linear infinite {ring.dir};
			"
		>
			<!-- Trail -->
			<div
				class="absolute inset-0 rounded-full"
				style="
					background: conic-gradient(from {ring.startDeg}deg, {ring.gradient});
					-webkit-mask: radial-gradient(farthest-side, transparent calc(100% - {ring.trail}px), #000 calc(100% - {ring.trail}px));
					mask: radial-gradient(farthest-side, transparent calc(100% - {ring.trail}px), #000 calc(100% - {ring.trail}px));
				"
			></div>
			<!-- Head dot — positioned at the gradient's 100% point -->
			<div
				class="absolute rounded-full"
				style="
					left: {dotX}px; top: {dotY}px;
					width: {ring.dot}px; height: {ring.dot}px;
					background: {ring.head};
					box-shadow: 0 0 10px {ring.head}, 0 0 24px {ring.color}, 0 0 44px {ring.glow};
				"
			></div>
		</div>

		<!-- Static base ring -->
		<div class="absolute rounded-full" style="width: {ring.size}px; height: {ring.size}px; border: 1px solid {ring.border};"></div>
	{/each}
	<span class="text-sm text-muted-foreground">Reconnecting...</span>
</div>
