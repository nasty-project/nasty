import { getContext, setContext, type Component } from "svelte";

export const THEMES = { light: "", dark: ".dark" } as const;

export type ChartConfig = {
	[k in string]: {
		label?: string;
		icon?: Component;
	} & (
		| { color?: string; theme?: never }
		| { color?: never; theme: Record<keyof typeof THEMES, string> }
	);
};

// One series entry from layerchart v2's tooltip state
// (`getChartContext().tooltip.series[number]`). Mirrors layerchart's internal
// `TooltipSeries` (not re-exported at the package root); replaces the old
// recharts-style payload item — note `label` where that had `name`, and there
// is no longer a per-item nested raw datum (that's the shared `tooltip.data`).
export type TooltipPayload = {
	key: string;
	label: string;
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	value: any;
	color?: string;
	visible?: boolean;
	// eslint-disable-next-line @typescript-eslint/no-explicit-any
	config?: any;
};

// Helper to extract item config from a tooltip series entry.
export function getPayloadConfigFromPayload(
	config: ChartConfig,
	payload: TooltipPayload,
	key: string
) {
	if (typeof payload !== "object" || payload === null) return undefined;

	let configLabelKey: string = key;

	if (payload.key === key) {
		configLabelKey = payload.key;
	} else if (payload.label === key) {
		configLabelKey = payload.label;
	}

	return configLabelKey in config ? config[configLabelKey] : config[key as keyof typeof config];
}

type ChartContextValue = {
	config: ChartConfig;
};

const chartContextKey = Symbol("chart-context");

export function setChartContext(value: ChartContextValue) {
	return setContext(chartContextKey, value);
}

export function useChart() {
	return getContext<ChartContextValue>(chartContextKey);
}
