// Ambient module declarations for d3 packages that ship without TypeScript type declarations
declare module 'd3-scale' {
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	export function scaleUtc(): any;
}

declare module 'd3-shape' {
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	export const curveMonotoneX: any;
}
