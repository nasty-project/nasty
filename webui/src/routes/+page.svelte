<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { goto } from '$app/navigation';
	import { getClient } from '$lib/client';
	import { withToast } from '$lib/toast.svelte';
	import { createIoHistory } from '$lib/history.svelte';
	import {
		dashboardPrefs,
		dashboardPresetTabVisible,
		dashboardPresets,
		dashboardWidgetColumnSpan,
		dashboardWidgetMeta,
		dashboardWidgetValidColumns,
		dashboardWidgetWidthClass,
		getActiveDashboardView,
		layoutDashboardWidgets,
		placeDashboardWidget,
		resolveDashboardDensity,
		resolveDashboardWidgets,
		selectDashboardView,
		setDashboardWidgetVisibility,
		updateActiveDashboardView,
		updateDashboardWidgetAppearance,
		type DashboardPreferences,
		type DashboardFixedPreset,
		type DashboardWidgetConfig,
		type DashboardWidgetId,
		type DashboardWidgetRowSpans,
	} from '$lib/dashboard.svelte';
	import type {
		ActiveAlert,
		App,
		AppsStatus,
		BackupScheduleEntry,
		DiskHealth,
		DiskIoStats,
		Filesystem,
		FsUsage,
		NetIfStats,
		ProtocolStatus,
		ResourceHistory,
		Settings,
		SystemHealth,
		SystemInfo,
		SystemStats,
		SystemStatus,
		VmStatus,
	} from '$lib/types';
	import {
		shouldPollDashboardHealth,
		summarizeEnabledServices,
		summarizeManagedContainers,
		type DashboardHealthFreshness,
		type ManagedContainerHealthSummary,
		type ServiceHealthSummary,
	} from '$lib/dashboard-health';
	import { Button } from '$lib/components/ui/button';
	import { Card, CardContent } from '$lib/components/ui/card';
	import AlertsWidget from '$lib/components/dashboard/alerts-widget.svelte';
	import ClockWidget from '$lib/components/dashboard/clock-widget.svelte';
	import ComputeWidget from '$lib/components/dashboard/compute-widget.svelte';
	import CustomizeDialog from '$lib/components/dashboard/customize-dialog.svelte';
	import DiskIoWidget from '$lib/components/dashboard/disk-io-widget.svelte';
	import HistoryControls from '$lib/components/dashboard/history-controls.svelte';
	import HealthWidget from '$lib/components/dashboard/health-widget.svelte';
	import HistoryWidget from '$lib/components/dashboard/history-widget.svelte';
	import NetworkWidget from '$lib/components/dashboard/network-widget.svelte';
	import OperationsWidget from '$lib/components/dashboard/operations-widget.svelte';
	import ScheduleWidget from '$lib/components/dashboard/schedule-widget.svelte';
	import StorageWidget from '$lib/components/dashboard/storage-widget.svelte';
	import SummaryWidget from '$lib/components/dashboard/summary-widget.svelte';
	import SystemWidget from '$lib/components/dashboard/system-widget.svelte';
	import WidgetOptions from '$lib/components/dashboard/widget-options.svelte';
	import { ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Check, GripVertical, Settings2, X } from '@lucide/svelte';

	type MetricsRange = '5m' | '1h' | '1d' | '7d' | '30d';
	type DiskRate = { readRate: number; writeRate: number };
	type NetworkRate = { rxRate: number; txRate: number };
	type ChartSample = { time: Date; in: number; out: number };

	const client = getClient();
	const rangeDurations: Record<MetricsRange, number> = {
		'5m': 300_000,
		'1h': 3_600_000,
		'1d': 86_400_000,
		'7d': 604_800_000,
		'30d': 2_592_000_000,
	};

	let info = $state<SystemInfo | null>(null);
	let health = $state<SystemHealth | null>(null);
	let stats = $state<SystemStats | null>(null);
	let filesystems = $state<Filesystem[]>([]);
	let alerts = $state<ActiveAlert[]>([]);
	let systemStatus = $state<SystemStatus | null>(null);
	let diskHealth = $state<DiskHealth[]>([]);
	let filesystemUsages = $state<Record<string, FsUsage>>({});
	let loading = $state(true);
	let filesystemsLoaded = $state(false);
	let infoLoaded = $state(false);
	let healthLoaded = $state(false);
	let alertsLoaded = $state(false);
	let operationsLoaded = $state(false);
	let serviceHealth = $state<ServiceHealthSummary | null>(null);
	let serviceHealthFreshness = $state<DashboardHealthFreshness>('loading');
	let containerHealth = $state<ManagedContainerHealthSummary | null>(null);
	let containerHealthFreshness = $state<DashboardHealthFreshness>('loading');
	let appsStatus = $state<AppsStatus | null>(null);
	let vms = $state<VmStatus[] | null>(null);
	let vmFreshness = $state<DashboardHealthFreshness>('loading');
	let settings = $state<Settings | null>(null);
	let clockDataLoaded = $state(false);
	let scheduleEntries = $state<BackupScheduleEntry[] | null>(null);
	let scheduleFreshness = $state<DashboardHealthFreshness>('loading');
	let customizeOpen = $state(false);
	let editingDashboard = $state(false);
	let customizeButton = $state<HTMLButtonElement | null>(null);
	let editDoneButton = $state<HTMLButtonElement | null>(null);
	let refreshTimer: ReturnType<typeof setInterval> | null = null;
	let refreshTick = 0;
	let refreshInFlight = false;
	let filesystemRequest = 0;
	let storageRequest = 0;
	let statsRequest = 0;
	let infoRequest = 0;
	let healthRequest = 0;
	let alertsRequest = 0;
	let operationsRequest = 0;
	let clockRequest = 0;
	let scheduleRequest = 0;
	let serviceHealthInFlight: Promise<void> | null = null;
	let containerHealthInFlight: Promise<void> | null = null;
	let vmHealthInFlight: Promise<void> | null = null;

	let metricsRange = $state<MetricsRange>('5m');
	let metricsOffset = $state(0);
	let previousDiskIo = $state<DiskIoStats[]>([]);
	let previousNetworkIo = $state<NetIfStats[]>([]);
	let previousSampleTime = $state(0);
	let diskRates = $state<Map<string, DiskRate>>(new Map());
	let networkRates = $state<Map<string, NetworkRate>>(new Map());
	let networkSamples = $state<Map<string, ChartSample[]>>(new Map());
	let diskSamples = $state<Map<string, ChartSample[]>>(new Map());
	let cpuSamples = $state<ChartSample[]>([]);
	let memorySamples = $state<ChartSample[]>([]);
	let historyLoading = $state(false);
	let historyRequest = 0;
	let draggedWidget = $state<DashboardWidgetId | null>(null);
	let dragTarget = $state<{ column: number; row: number } | null>(null);
	let dragOrigin: { column: number; row: number } | null = null;
	let dragOffset = { x: 0, y: 0 };
	let dragRowMetrics = $state({ height: 8, gap: 16 });
	let widgetRowSpans = $state<DashboardWidgetRowSpans>({});
	let movementAnnouncement = $state('');

	const networkHistory = createIoHistory();
	const diskHistory = createIoHistory();
	const cpuHistory = createIoHistory();
	const memoryHistory = createIoHistory();

	let widgets = $derived(resolveDashboardWidgets(dashboardPrefs.value));
	let density = $derived(resolveDashboardDensity(dashboardPrefs.value));
	let activeCustomView = $derived(getActiveDashboardView(dashboardPrefs.value));
	let gridLayout = $derived(layoutDashboardWidgets(widgets, widgetRowSpans));
	let draggedConfig = $derived(widgets.find((widget) => widget.id === draggedWidget));
	let validDropColumns = $derived(draggedConfig ? dashboardWidgetValidColumns(draggedConfig.width) : []);
	let dragLayout = $derived(draggedWidget && dragTarget
		? layoutDashboardWidgets(widgets, widgetRowSpans, { id: draggedWidget, ...dragTarget })
		: null
	);
	let dragPreview = $derived(draggedWidget && dragLayout ? dragLayout[draggedWidget] : null);
	let presetLabel = $derived(
		dashboardPrefs.value.preset === 'custom'
			? activeCustomView.name
			: dashboardPresets[dashboardPrefs.value.preset].label
	);
	let dashboardSelection = $derived(
		dashboardPrefs.value.preset === 'custom'
			? `custom:${activeCustomView.id}`
			: dashboardPrefs.value.preset
	);
	let presetTabs = $derived(
		(Object.entries(dashboardPresets) as [DashboardFixedPreset, (typeof dashboardPresets)[DashboardFixedPreset]][])
			.filter(([id]) => dashboardPresetTabVisible(dashboardPrefs.value, id))
	);

	function hasWidget(id: DashboardWidgetId): boolean {
		return widgets.some((widget) => widget.id === id);
	}

	function needsStats(): boolean {
		return widgets.some((widget) => ['cpu_load', 'memory_usage', 'cpu_status', 'storage', 'history', 'network', 'disk_io'].includes(widget.id));
	}

	function needsMetricsHistory(): boolean {
		return widgets.some((widget) => ['history', 'network', 'disk_io'].includes(widget.id));
	}

	function healthPollingEnabled(id: 'service_health' | 'container_health'): boolean {
		const visible = hasWidget(id) || (id === 'container_health' && hasWidget('compute'));
		return shouldPollDashboardHealth(visible, document.hidden);
	}

	function vmPollingEnabled(): boolean {
		return shouldPollDashboardHealth(hasWidget('compute'), document.hidden);
	}

	function masonryItem(node: HTMLElement, id: DashboardWidgetId) {
		let frame = 0;
		const update = () => {
			cancelAnimationFrame(frame);
			frame = requestAnimationFrame(() => {
				const gridStyle = getComputedStyle(node.parentElement!);
				const rowHeight = Number.parseFloat(gridStyle.gridAutoRows) || 8;
				const gap = Number.parseFloat(gridStyle.rowGap) || 0;
				const rows = Math.ceil((node.getBoundingClientRect().height + gap) / (rowHeight + gap));
				const rowSpan = Math.max(1, rows);
				if (widgetRowSpans[id] !== rowSpan) widgetRowSpans = { ...widgetRowSpans, [id]: rowSpan };
			});
		};
		const observer = new ResizeObserver(update);
		observer.observe(node);
		update();
		return {
			destroy() {
				cancelAnimationFrame(frame);
				observer.disconnect();
			},
		};
	}

	function saveWidgetPlacement(id: DashboardWidgetId, column: number, row: number) {
		const preferences = dashboardPrefs.value;
		if (preferences.preset !== 'custom') return;
		dashboardPrefs.set(updateActiveDashboardView(preferences, {
			widgets: placeDashboardWidget(getActiveDashboardView(preferences).widgets, id, column, row),
		}));
	}

	function updateWidgetAppearance(
		id: DashboardWidgetId,
		patch: Partial<Pick<DashboardWidgetConfig, 'width' | 'presentation'>>,
	) {
		const preferences = dashboardPrefs.value;
		if (preferences.preset !== 'custom') return;
		const reloadHistory = id === 'history' && patch.presentation === 'tiny' && metricsOffset > 0;
		if (reloadHistory) metricsOffset = 0;
		dashboardPrefs.set(updateActiveDashboardView(preferences, {
			widgets: updateDashboardWidgetAppearance(getActiveDashboardView(preferences).widgets, id, patch),
		}));
		if (reloadHistory) void loadMetrics(true);
	}

	async function hideWidget(id: DashboardWidgetId) {
		const preferences = dashboardPrefs.value;
		if (preferences.preset !== 'custom' || widgets.length <= 1) return;
		const index = widgets.findIndex((widget) => widget.id === id);
		if (index < 0) return;
		const nextWidgetId = widgets[index + 1]?.id ?? widgets[index - 1]?.id;
		const activeView = getActiveDashboardView(preferences);
		dashboardPrefs.set(updateActiveDashboardView(preferences, {
			widgets: setDashboardWidgetVisibility(activeView.widgets, id, false),
		}));
		announceMovement(`${dashboardWidgetMeta[id].label} removed from ${activeView.name}.`);
		await tick();
		const nextRemoveButton = document.querySelector<HTMLButtonElement>(`[data-dashboard-remove="${nextWidgetId}"]`);
		if (nextRemoveButton && !nextRemoveButton.disabled) nextRemoveButton.focus();
		else editDoneButton?.focus();
	}

	async function beginDashboardEditing() {
		if (dashboardPrefs.value.preset !== 'custom') {
			customizeOpen = true;
			return;
		}
		editingDashboard = true;
		await tick();
		editDoneButton?.focus();
	}

	async function finishDashboardEditing() {
		editingDashboard = false;
		endWidgetDrag();
		await tick();
		customizeButton?.focus();
	}

	function announceMovement(message: string) {
		movementAnnouncement = '';
		requestAnimationFrame(() => movementAnnouncement = message);
	}

	function moveCustomWidget(id: DashboardWidgetId, horizontal: -1 | 0 | 1, vertical: -1 | 0 | 1) {
		const position = gridLayout[id];
		const widget = widgets.find((candidate) => candidate.id === id);
		if (!position || !widget) return;
		const column = position.column + horizontal * dashboardWidgetColumnSpan(widget.width);
		let row = vertical < 0
			? Math.max(0, widget.row - position.rowSpan)
			: position.row + vertical * position.rowSpan;
		if (column < 0 || column + position.columnSpan > 12) return;
		let proposed = layoutDashboardWidgets(widgets, widgetRowSpans, { id, column, row })[id];
		while (vertical < 0 && proposed && proposed.row >= position.row && row > 0) {
			row = Math.max(0, row - position.rowSpan);
			proposed = layoutDashboardWidgets(widgets, widgetRowSpans, { id, column, row })[id];
		}
		if (!proposed || (proposed.column === position.column && proposed.row === position.row)) return;
		saveWidgetPlacement(id, column, row);
		announceMovement(`${dashboardWidgetMeta[id].label} moved ${horizontal < 0 ? 'left' : horizontal > 0 ? 'right' : vertical < 0 ? 'up' : 'down'}.`);
	}

	function moveCustomWidgetInOrder(id: DashboardWidgetId, direction: -1 | 1) {
		const preferences = dashboardPrefs.value;
		if (preferences.preset !== 'custom') return;
		const target = widgets[widgets.findIndex((widget) => widget.id === id) + direction];
		if (!target) return;
		const activeWidgets = getActiveDashboardView(preferences).widgets.map((widget) => ({ ...widget }));
		const sourceIndex = activeWidgets.findIndex((widget) => widget.id === id);
		const targetIndex = activeWidgets.findIndex((widget) => widget.id === target.id);
		[activeWidgets[sourceIndex], activeWidgets[targetIndex]] = [activeWidgets[targetIndex], activeWidgets[sourceIndex]];
		dashboardPrefs.set(updateActiveDashboardView(preferences, { widgets: activeWidgets }));
		announceMovement(`${dashboardWidgetMeta[id].label} moved ${direction < 0 ? 'earlier' : 'later'} in the stacked layout.`);
	}

	function startWidgetDrag(event: DragEvent, id: DashboardWidgetId) {
		draggedWidget = id;
		const position = gridLayout[id];
		dragTarget = position ? { column: position.column, row: position.row } : null;
		dragOrigin = dragTarget;
		const widget = (event.currentTarget as HTMLElement).closest<HTMLElement>('[data-dashboard-widget]');
		const rect = widget?.getBoundingClientRect();
		dragOffset = rect ? { x: event.clientX - rect.left, y: event.clientY - rect.top } : { x: 0, y: 0 };
		event.dataTransfer?.setData('text/plain', id);
		if (event.dataTransfer) event.dataTransfer.effectAllowed = 'move';
	}

	function targetGridDrag(event: DragEvent) {
		if (!draggedWidget || !draggedConfig) return;
		event.preventDefault();
		if (event.dataTransfer) event.dataTransfer.dropEffect = 'move';
		const grid = event.currentTarget as HTMLElement;
		const rect = grid.getBoundingClientRect();
		const style = getComputedStyle(grid);
		const rowHeight = Number.parseFloat(style.gridAutoRows) || 8;
		const rowGap = Number.parseFloat(style.rowGap) || 0;
		dragRowMetrics = { height: rowHeight, gap: rowGap };
		const columnGap = Number.parseFloat(style.columnGap) || 0;
		const columnWidth = (rect.width - columnGap * 11) / 12;
		const requestedLeft = event.clientX - rect.left - dragOffset.x;
		const column = dashboardWidgetValidColumns(draggedConfig.width).reduce((nearest, candidate) =>
			Math.abs(candidate * (columnWidth + columnGap) - requestedLeft) < Math.abs(nearest * (columnWidth + columnGap) - requestedLeft)
				? candidate
				: nearest
		);
		dragTarget = {
			column,
			row: Math.max(0, Math.round((event.clientY - rect.top - dragOffset.y) / (rowHeight + rowGap))),
		};
	}

	function dropWidget(event: DragEvent) {
		event.preventDefault();
		if (draggedWidget && dragTarget && dragPreview && dragOrigin
			&& (dragTarget.column !== dragOrigin.column || dragTarget.row !== dragOrigin.row)
			&& (dragPreview.column !== dragOrigin.column || dragPreview.row !== dragOrigin.row)) {
			saveWidgetPlacement(draggedWidget, dragTarget.column, dragTarget.row);
			announceMovement(`${dashboardWidgetMeta[draggedWidget].label} moved to grid column ${dragTarget.column + 1}.`);
		}
		endWidgetDrag();
	}

	function endWidgetDrag() {
		draggedWidget = null;
		dragTarget = null;
		dragOrigin = null;
	}

	function widgetGridStyle(widget: DashboardWidgetConfig): string {
		const position = gridLayout[widget.id];
		return position
			? `--dashboard-column: ${position.column + 1}; --dashboard-row: ${position.row + 1}; grid-row-end: span ${position.rowSpan};`
			: '';
	}

	function previewTop(row: number): number {
		return row * (dragRowMetrics.height + dragRowMetrics.gap);
	}

	function previewHeight(rowSpan: number): number {
		return rowSpan * dragRowMetrics.height + Math.max(0, rowSpan - 1) * dragRowMetrics.gap;
	}

	function handleEvent(_: string, params: unknown) {
		const event = params as { collection?: string };
		if (event?.collection === 'filesystem') void loadFilesystemData();
	}

	onMount(() => {
		client.onEvent(handleEvent);
		const handleVisibilityChange = () => {
			if (healthPollingEnabled('service_health')) void loadServiceHealth(true);
			if (healthPollingEnabled('container_health')) void loadContainerHealth(true);
			if (vmPollingEnabled()) void loadVmHealth(true);
		};
		document.addEventListener('visibilitychange', handleVisibilityChange);
		void loadVisibleData(true).finally(() => loading = false);
		refreshTimer = setInterval(refreshVisibleData, 15_000);
		return () => {
			client.offEvent(handleEvent);
			document.removeEventListener('visibilitychange', handleVisibilityChange);
			if (refreshTimer) clearInterval(refreshTimer);
		};
	});

	async function loadVisibleData(showErrors: boolean) {
		const load = async () => {
			const tasks: Promise<unknown>[] = [
				fetchFilesystemInventory(),
			];
			if (hasWidget('system') || hasWidget('clock') || hasWidget('schedule')) {
				tasks.push(hasWidget('clock') ? loadClockData() : loadSystemInfo());
			}
			if (hasWidget('system')) {
				tasks.push(loadSystemHealth());
			}
			if (needsStats()) {
				const request = ++statsRequest;
				tasks.push(client.call<SystemStats>('system.stats').then((value) => {
					if (request !== statsRequest) return;
					stats = value;
					previousDiskIo = value.disk_io;
					previousNetworkIo = value.network;
					previousSampleTime = Date.now();
				}));
			}
			if (hasWidget('alerts')) tasks.push(loadAlerts());
			if (hasWidget('operations')) tasks.push(loadOperations());
			if (hasWidget('schedule')) tasks.push(loadBackupSchedule());
			if (healthPollingEnabled('service_health')) tasks.push(loadServiceHealth(true));
			if (healthPollingEnabled('container_health')) tasks.push(loadContainerHealth(true));
			if (vmPollingEnabled()) tasks.push(loadVmHealth(true));
			const results = await Promise.allSettled(tasks);
			if (hasWidget('storage')) await loadStorageDetails();
			if (needsMetricsHistory()) await loadMetrics();
			const failure = results.find((result): result is PromiseRejectedResult => result.status === 'rejected');
			if (failure) throw failure.reason;
		};

		if (showErrors) await withToast(load);
		else {
			try { await load(); } catch { /* Keep the last good widget data. */ }
		}
	}

	async function fetchFilesystemInventory() {
		const request = ++filesystemRequest;
		const value = await client.call<Filesystem[]>('fs.list');
		if (request !== filesystemRequest) return;
		filesystems = value;
		filesystemsLoaded = true;
	}

	async function loadSystemInfo() {
		const request = ++infoRequest;
		const value = await client.call<SystemInfo>('system.info');
		if (request !== infoRequest) return;
		info = value;
		infoLoaded = true;
	}

	async function loadClockData() {
		const request = ++clockRequest;
		const [nextInfo, nextSettings] = await Promise.allSettled([
			client.call<SystemInfo>('system.info'),
			client.call<Settings>('system.settings.get'),
		]);
		if (request !== clockRequest) return;
		if (nextInfo.status === 'fulfilled') {
			info = nextInfo.value;
			infoLoaded = true;
		}
		if (nextSettings.status === 'fulfilled') {
			settings = { ...nextSettings.value, dashboard_motd: nextSettings.value.dashboard_motd ?? '' };
		}
		clockDataLoaded = true;
		const failure = [nextInfo, nextSettings].find((result): result is PromiseRejectedResult => result.status === 'rejected');
		if (failure) throw failure.reason;
	}

	async function loadBackupSchedule() {
		const request = ++scheduleRequest;
		scheduleFreshness = scheduleEntries ? 'refreshing' : 'loading';
		try {
			const entries = await client.call<BackupScheduleEntry[]>('backup.schedule.list');
			if (request !== scheduleRequest) return;
			scheduleEntries = entries;
			scheduleFreshness = 'current';
		} catch {
			if (request === scheduleRequest) scheduleFreshness = scheduleEntries ? 'stale' : 'unavailable';
		}
	}

	async function loadSystemHealth() {
		const request = ++healthRequest;
		const value = await client.call<SystemHealth>('system.health');
		if (request !== healthRequest) return;
		health = value;
		healthLoaded = true;
	}

	async function loadAlerts() {
		const request = ++alertsRequest;
		const value = await client.call<ActiveAlert[]>('system.alerts');
		if (request !== alertsRequest) return;
		alerts = value;
		alertsLoaded = true;
	}

	async function loadOperations() {
		const request = ++operationsRequest;
		const value = await client.call<SystemStatus>('system.status');
		if (request !== operationsRequest) return;
		systemStatus = value;
		operationsLoaded = true;
	}

	function loadServiceHealth(markExistingRefreshing = false): Promise<void> {
		if (serviceHealthInFlight) return serviceHealthInFlight;
		if (markExistingRefreshing && serviceHealth) serviceHealthFreshness = 'refreshing';
		if (!serviceHealth) serviceHealthFreshness = 'loading';

		const request = fetchServiceHealth();
		serviceHealthInFlight = request;
		void request.finally(() => {
			if (serviceHealthInFlight === request) serviceHealthInFlight = null;
		});
		return request;
	}

	async function fetchServiceHealth() {
		try {
			serviceHealth = summarizeEnabledServices(await client.call<ProtocolStatus[]>('service.protocol.list'));
			serviceHealthFreshness = 'current';
		} catch {
			serviceHealthFreshness = serviceHealth ? 'stale' : 'unavailable';
		}
	}

	function loadContainerHealth(markExistingRefreshing = false): Promise<void> {
		if (containerHealthInFlight) return containerHealthInFlight;
		if (markExistingRefreshing && containerHealth) containerHealthFreshness = 'refreshing';
		if (!containerHealth) containerHealthFreshness = 'loading';

		const request = fetchContainerHealth();
		containerHealthInFlight = request;
		void request.finally(() => {
			if (containerHealthInFlight === request) containerHealthInFlight = null;
		});
		return request;
	}

	async function fetchContainerHealth() {
		let status: AppsStatus;
		try {
			status = await client.call<AppsStatus>('apps.status');
		} catch {
			containerHealthFreshness = containerHealth && appsStatus ? 'stale' : 'unavailable';
			return;
		}

		if (!status.enabled) {
			appsStatus = status;
			containerHealth = summarizeManagedContainers(status, []);
			containerHealthFreshness = 'current';
			return;
		}

		try {
			const nextContainerHealth = summarizeManagedContainers(status, await client.call<App[]>('apps.list'));
			appsStatus = status;
			containerHealth = nextContainerHealth;
			containerHealthFreshness = 'current';
		} catch {
			if (!status.running) {
				appsStatus = status;
				containerHealth = { runtime: 'down', expected: null, running: 0 };
				containerHealthFreshness = 'current';
			} else if (containerHealth?.expected != null && appsStatus?.enabled === status.enabled && appsStatus.running === status.running) {
				containerHealthFreshness = 'stale';
			} else {
				appsStatus = status;
				containerHealth = { runtime: 'running', expected: null, running: null };
				containerHealthFreshness = 'unavailable';
			}
		}
	}

	function loadVmHealth(markExistingRefreshing = false): Promise<void> {
		if (vmHealthInFlight) return vmHealthInFlight;
		if (markExistingRefreshing && vms) vmFreshness = 'refreshing';
		if (!vms) vmFreshness = 'loading';

		const request = fetchVmHealth();
		vmHealthInFlight = request;
		void request.finally(() => {
			if (vmHealthInFlight === request) vmHealthInFlight = null;
		});
		return request;
	}

	async function fetchVmHealth() {
		try {
			vms = await client.call<VmStatus[]>('vm.list');
			vmFreshness = 'current';
		} catch {
			vmFreshness = vms ? 'stale' : 'unavailable';
		}
	}

	async function loadFilesystemData() {
		try {
			await fetchFilesystemInventory();
			if (hasWidget('storage')) await loadStorageDetails();
		} catch { /* Keep the last good filesystem inventory. */ }
	}

	async function loadStorageDetails() {
		const request = ++storageRequest;
		const mountedFilesystems = filesystems.filter((filesystem) => filesystem.mounted);
		const [freshHealth, usageEntries] = await Promise.all([
			client.call<DiskHealth[]>('system.disks').catch(() => null),
			Promise.all(mountedFilesystems.map(async (filesystem) => {
				try {
					return [filesystem.name, await client.call<FsUsage>('fs.usage', { name: filesystem.name })] as const;
				} catch {
					return null;
				}
			})),
		]);
		if (request !== storageRequest) return;
		if (freshHealth) diskHealth = freshHealth;
		const mountedNames = new Set(mountedFilesystems.map((filesystem) => filesystem.name));
		const retainedUsages = Object.fromEntries(Object.entries(filesystemUsages).filter(([name]) => mountedNames.has(name)));
		filesystemUsages = {
			...retainedUsages,
			...Object.fromEntries(usageEntries.filter((entry): entry is readonly [string, FsUsage] => entry !== null)),
		};
	}

	async function refreshVisibleData() {
		if (healthPollingEnabled('service_health')) void loadServiceHealth();
		if (healthPollingEnabled('container_health')) void loadContainerHealth();
		if (vmPollingEnabled()) void loadVmHealth();
		if (refreshInFlight) return;
		refreshInFlight = true;
		refreshTick += 1;
		try {
			const tasks: Promise<unknown>[] = [];
			if (needsStats()) tasks.push(refreshStats());
			if (hasWidget('alerts')) tasks.push(loadAlerts());
			if (hasWidget('operations')) tasks.push(loadOperations());
			if (refreshTick % 4 === 0) {
				if (hasWidget('clock')) tasks.push(loadClockData());
				else if (hasWidget('schedule') || (hasWidget('system') && !infoLoaded)) tasks.push(loadSystemInfo());
				if (hasWidget('system')) tasks.push(loadSystemHealth());
				if (hasWidget('schedule')) tasks.push(loadBackupSchedule());
			}
			if (refreshTick % 4 === 0) tasks.push(loadFilesystemData());
			else if (hasWidget('storage') && refreshTick % 2 === 0) tasks.push(loadStorageDetails());
			await Promise.all(tasks);
		} catch {
			/* Polling is best effort. */
		} finally {
			refreshInFlight = false;
		}
	}

	async function refreshStats() {
		const request = ++statsRequest;
		const next = await client.call<SystemStats>('system.stats');
		if (request !== statsRequest) return;
		const now = Date.now();
		const elapsed = (now - previousSampleTime) / 1000;
		const sampleTime = new Date(now);

		if (previousSampleTime > 0 && elapsed > 0) {
			const nextDiskRates = new Map<string, DiskRate>();
			for (const current of next.disk_io) {
				const previous = previousDiskIo.find((device) => device.name === current.name);
				if (!previous) continue;
				const readRate = Math.max(0, (current.read_bytes - previous.read_bytes) / elapsed);
				const writeRate = Math.max(0, (current.write_bytes - previous.write_bytes) / elapsed);
				nextDiskRates.set(current.name, { readRate, writeRate });
				if (metricsRange === '5m' && metricsOffset === 0) diskHistory.push(current.name, sampleTime, readRate, writeRate);
			}
			diskRates = nextDiskRates;
			if (metricsRange === '5m' && metricsOffset === 0) {
				diskSamples = new Map(next.disk_io.map((device) => [device.name, liveSamples(diskHistory.getSamples(device.name), sampleTime)]));
			}

			const nextNetworkRates = new Map<string, NetworkRate>();
			for (const current of next.network) {
				const previous = previousNetworkIo.find((iface) => iface.name === current.name);
				if (!previous) continue;
				const rxRate = Math.max(0, (current.rx_bytes - previous.rx_bytes) / elapsed);
				const txRate = Math.max(0, (current.tx_bytes - previous.tx_bytes) / elapsed);
				nextNetworkRates.set(current.name, { rxRate, txRate });
				if (metricsRange === '5m' && metricsOffset === 0) networkHistory.push(current.name, sampleTime, rxRate, txRate);
			}
			networkRates = nextNetworkRates;
			if (metricsRange === '5m' && metricsOffset === 0) {
				networkSamples = new Map(next.network.map((iface) => [iface.name, liveSamples(networkHistory.getSamples(iface.name), sampleTime)]));
			}

			if (metricsRange === '5m' && metricsOffset === 0) {
				const cpuPercent = Math.min(100, (next.cpu.load_1 / next.cpu.count) * 100);
				const memoryPercent = next.memory.total_bytes > 0 ? next.memory.used_bytes / next.memory.total_bytes * 100 : 0;
				cpuHistory.push('cpu', sampleTime, cpuPercent, 0);
				memoryHistory.push('memory', sampleTime, memoryPercent, 0);
				cpuSamples = liveSamples(cpuHistory.getSamples('cpu'), sampleTime);
				memorySamples = liveSamples(memoryHistory.getSamples('memory'), sampleTime);
			}
		}

		previousDiskIo = next.disk_io;
		previousNetworkIo = next.network;
		previousSampleTime = now;
		stats = next;
	}

	function liveSamples(samples: ChartSample[], now: Date): ChartSample[] {
		const cutoff = now.getTime() - rangeDurations['5m'];
		return samples.filter((sample) => sample.time.getTime() >= cutoff);
	}

	function applyHistory(
		network: ResourceHistory[] | null,
		disk: ResourceHistory[] | null,
		cpu: ResourceHistory[] | null,
		memory: ResourceHistory[] | null,
	) {
		if (network) {
			networkHistory.clear();
			for (const resource of network) for (const sample of resource.samples) networkHistory.push(resource.name, new Date(sample.ts), sample.in_rate, sample.out_rate);
			networkSamples = new Map(network.map((resource) => [resource.name, [...networkHistory.getSamples(resource.name)]]));
		}
		if (disk) {
			diskHistory.clear();
			for (const resource of disk) for (const sample of resource.samples) diskHistory.push(resource.name, new Date(sample.ts), sample.in_rate, sample.out_rate);
			diskSamples = new Map(disk.map((resource) => [resource.name, [...diskHistory.getSamples(resource.name)]]));
		}
		if (cpu) {
			cpuHistory.clear();
			for (const resource of cpu) for (const sample of resource.samples) cpuHistory.push('cpu', new Date(sample.ts), sample.in_rate, 0);
			cpuSamples = [...cpuHistory.getSamples('cpu')];
		}
		if (memory) {
			memoryHistory.clear();
			for (const resource of memory) for (const sample of resource.samples) memoryHistory.push('memory', new Date(sample.ts), sample.in_rate, 0);
			memorySamples = [...memoryHistory.getSamples('memory')];
		}
	}

	async function loadMetrics(clearFailedSeries = false) {
		const request = ++historyRequest;
		historyLoading = true;
		try {
			const params = { range: metricsRange, ...(metricsOffset > 0 ? { offset: metricsOffset } : {}) };
			const [network, disk, cpu, memory] = await Promise.allSettled([
				hasWidget('network') ? client.call<ResourceHistory[]>('system.metrics.history', { kind: 'net', ...params }) : Promise.resolve([]),
				hasWidget('disk_io') ? client.call<ResourceHistory[]>('system.metrics.history', { kind: 'disk', ...params }) : Promise.resolve([]),
				hasWidget('history') ? client.call<ResourceHistory[]>('system.metrics.history', { kind: 'cpu', ...params }) : Promise.resolve([]),
				hasWidget('history') ? client.call<ResourceHistory[]>('system.metrics.history', { kind: 'mem', ...params }) : Promise.resolve([]),
			]);
			if (request === historyRequest) applyHistory(
				network.status === 'fulfilled' ? network.value : clearFailedSeries ? [] : null,
				disk.status === 'fulfilled' ? disk.value : clearFailedSeries ? [] : null,
				cpu.status === 'fulfilled' ? cpu.value : clearFailedSeries ? [] : null,
				memory.status === 'fulfilled' ? memory.value : clearFailedSeries ? [] : null,
			);
		} catch {
			/* History fills from live samples when unavailable. */
		} finally {
			if (request === historyRequest) historyLoading = false;
		}
	}

	async function changeRange(range: MetricsRange) {
		metricsRange = range;
		metricsOffset = 0;
		await loadMetrics(true);
	}

	async function navigateBack() {
		metricsOffset += rangeDurations[metricsRange];
		await loadMetrics(true);
	}

	async function navigateForward() {
		metricsOffset = Math.max(0, metricsOffset - rangeDurations[metricsRange]);
		await loadMetrics(true);
	}

	async function navigateLive() {
		metricsOffset = 0;
		await loadMetrics(true);
	}

	async function applyPreferences(preferences: DashboardPreferences, enterEditing = false) {
		if (resolveDashboardWidgets(preferences).some((widget) => widget.id === 'history' && widget.presentation === 'tiny')) {
			metricsOffset = 0;
		}
		const wasEditing = editingDashboard;
		dashboardPrefs.set(preferences);
		editingDashboard = preferences.preset === 'custom' && (wasEditing || enterEditing);
		await tick();
		if (enterEditing) {
			requestAnimationFrame(() => (editingDashboard ? editDoneButton : customizeButton)?.focus());
		}
		await loadVisibleData(false);
	}

	async function switchDashboard(selection: string) {
		const preferences = dashboardPrefs.value;
		let next: DashboardPreferences = preferences;
		if (selection.startsWith('custom:')) {
			next = selectDashboardView(preferences, selection.slice('custom:'.length));
		} else if (selection === 'storage' || selection === 'monitoring' || selection === 'overview') {
			next = { ...preferences, preset: selection };
		}
		if (next === preferences) return;
		if (next.preset !== 'custom') editingDashboard = false;
		await applyPreferences(next);
	}

	function handleDashboardTabKeydown(event: KeyboardEvent) {
		if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
		const target = event.currentTarget as HTMLButtonElement;
		const tabs = Array.from(target.parentElement?.querySelectorAll<HTMLButtonElement>('[role="tab"]') ?? []);
		const current = tabs.indexOf(target);
		if (current < 0 || tabs.length === 0) return;
		const next = event.key === 'Home' ? 0
			: event.key === 'End' ? tabs.length - 1
			: (current + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
		event.preventDefault();
		tabs[next].focus();
		tabs[next].click();
	}
</script>

<div class="mb-4">
	<div class="flex flex-wrap items-center justify-between gap-3">
		<div>
			<div class="text-sm font-semibold">{presetLabel} dashboard</div>
			<div class="text-xs text-muted-foreground">{widgets.length} visible widget{widgets.length === 1 ? '' : 's'} - {density} density</div>
			{#if editingDashboard}<div class="text-xs text-muted-foreground">Move widgets with the direction controls or drag handle. Use each options menu to change its size or presentation, or X to remove it from this view.</div>{/if}
		</div>
		{#if editingDashboard}
			<div class="flex items-center gap-2">
				<Button variant="outline" size="sm" onclick={() => customizeOpen = true}><Settings2 /> Widgets & views</Button>
				<Button bind:ref={editDoneButton} size="sm" onclick={() => void finishDashboardEditing()}><Check /> Done</Button>
			</div>
		{:else}
			<Button bind:ref={customizeButton} variant="outline" size="sm" onclick={() => void beginDashboardEditing()}><Settings2 /> Customize</Button>
		{/if}
	</div>
	<div class="mt-3 overflow-x-auto overflow-y-hidden border-b border-border">
		<div class="flex min-w-max items-end" role="tablist" aria-label="Dashboard views">
			{#each presetTabs as [id, preset]}
				<button type="button" role="tab" aria-selected={dashboardSelection === id} aria-controls="dashboard-panel" tabindex={dashboardSelection === id ? 0 : -1} onclick={() => void switchDashboard(id)} onkeydown={handleDashboardTabKeydown} class="-mb-px whitespace-nowrap border-b-2 px-3 py-2 text-sm font-medium transition-colors {dashboardSelection === id ? 'border-primary text-foreground' : 'border-transparent text-muted-foreground hover:border-border hover:text-foreground'}">{preset.label}</button>
			{/each}
			{#if presetTabs.length > 0}<span class="mx-1 mb-2 h-5 w-px bg-border" aria-hidden="true"></span>{/if}
			{#each dashboardPrefs.value.customViews as view (view.id)}
				{@const selection = `custom:${view.id}`}
				<button type="button" role="tab" aria-selected={dashboardSelection === selection} aria-controls="dashboard-panel" tabindex={dashboardSelection === selection ? 0 : -1} onclick={() => void switchDashboard(selection)} onkeydown={handleDashboardTabKeydown} class="-mb-px whitespace-nowrap border-b-2 px-3 py-2 text-sm font-medium transition-colors {dashboardSelection === selection ? 'border-primary text-foreground' : 'border-transparent text-muted-foreground hover:border-border hover:text-foreground'}">{view.name}</button>
			{/each}
		</div>
	</div>
</div>
<div class="sr-only" aria-live="polite">{movementAnnouncement}</div>

<div id="dashboard-panel" role="tabpanel" aria-label={`${presetLabel} dashboard`}>
{#if filesystemsLoaded && filesystems.length === 0}
	<Card class="mb-4 border-primary/30 bg-primary/5">
		<CardContent class="flex flex-wrap items-center gap-6 py-6">
			<div class="min-w-0 flex-1"><h2 class="text-lg font-bold">Get started with NASty</h2><p class="mt-1 text-sm text-muted-foreground">Create your first filesystem to start storing and sharing data.</p></div>
			<Button onclick={() => goto('/filesystems?create')}>Create filesystem</Button>
		</CardContent>
	</Card>
{/if}

{#if loading}
	<Card><CardContent class="py-10 text-center text-sm text-muted-foreground">Loading dashboard...</CardContent></Card>
{:else}
	<div class="relative grid grid-cols-12 auto-rows-[8px] gap-4" role="group" aria-label="Dashboard widget grid" ondragover={targetGridDrag} ondrop={dropWidget}>
		{#if draggedWidget && draggedConfig}
			<div class="pointer-events-none absolute inset-0 z-20 hidden grid-cols-12 grid-rows-1 gap-4 xl:grid" aria-hidden="true">
				{#each validDropColumns as column}
					<div
						class="h-full rounded-lg border border-dashed transition-colors {dragTarget?.column === column ? 'border-primary/70 bg-primary/10' : 'border-border/60 bg-muted/10'}"
						style={`grid-column: ${column + 1} / span ${dashboardWidgetColumnSpan(draggedConfig.width)}; grid-row: 1;`}
					></div>
				{/each}
				{#if dragPreview}
					<div class="relative h-full" style={`grid-column: ${dragPreview.column + 1} / span ${dragPreview.columnSpan}; grid-row: 1;`}>
						<div class="absolute inset-x-0 rounded-lg border-2 border-primary bg-primary/15 shadow-lg" style={`top: ${previewTop(dragPreview.row)}px; height: ${previewHeight(dragPreview.rowSpan)}px;`}></div>
					</div>
				{/if}
			</div>
		{/if}
		{#each widgets as widget (widget.id)}
			{@const position = gridLayout[widget.id]}
			{@const persistentHeader = widget.id === 'history' && widget.presentation === 'standard'}
			<div
				use:masonryItem={widget.id}
				role="group"
				aria-label={dashboardWidgetMeta[widget.id].label}
				data-dashboard-widget={widget.id}
				style={widgetGridStyle(widget)}
				class="relative self-start rounded-lg transition-[opacity,box-shadow] {dashboardWidgetWidthClass(widget.id, widget.width)} {dashboardPrefs.value.preset === 'custom' ? 'dashboard-positioned' : ''} {draggedWidget === widget.id ? 'opacity-45' : ''}"
			>
				{#if persistentHeader}
					<div class="mb-1 flex min-h-8 flex-wrap items-center gap-1 text-muted-foreground xl:flex-nowrap {editingDashboard ? 'invisible pointer-events-none' : ''}" aria-hidden={editingDashboard}>
						<span class="text-[0.65rem] font-medium uppercase tracking-wide">History{historyLoading ? ' - loading' : ''}</span>
						{#if metricsOffset > 0}
							<span class="ml-1 min-w-0 truncate text-xs normal-case tracking-normal" title={`${new Date(Date.now() - metricsOffset - rangeDurations[metricsRange]).toLocaleString()} - ${new Date(Date.now() - metricsOffset).toLocaleString()}`}>{new Date(Date.now() - metricsOffset - rangeDurations[metricsRange]).toLocaleString()} - {new Date(Date.now() - metricsOffset).toLocaleString()}</span>
						{/if}
						<HistoryControls range={metricsRange} offset={metricsOffset} loading={historyLoading} class="ml-auto" onRange={(range) => void changeRange(range)} onBack={() => void navigateBack()} onForward={() => void navigateForward()} onLive={() => void navigateLive()} />
					</div>
				{/if}
				{#if editingDashboard}
					<!-- Edit-only chrome must not affect the masonry geometry users are arranging. -->
					<div role="group" aria-label={`${dashboardWidgetMeta[widget.id].label} edit controls`} class="absolute -top-4 right-2 z-10 flex min-h-7 max-w-[calc(100%-1rem)] items-center gap-1 rounded-md border border-border bg-background/95 px-1 text-muted-foreground shadow-sm focus-within:z-50">
						<span class="hidden truncate text-[0.65rem] font-medium uppercase tracking-wide 2xl:inline">{dashboardWidgetMeta[widget.id].label}{widget.id === 'history' && historyLoading ? ' - loading' : ''}</span>
						<div class="ml-auto flex items-center">
							<div class="flex xl:hidden">
								<button type="button" onclick={() => moveCustomWidgetInOrder(widget.id, -1)} disabled={widgets[0]?.id === widget.id} class="rounded p-1.5 hover:bg-accent hover:text-foreground disabled:opacity-20" aria-label={`Move ${dashboardWidgetMeta[widget.id].label} earlier`}><ArrowUp class="h-3.5 w-3.5" /></button>
								<button type="button" onclick={() => moveCustomWidgetInOrder(widget.id, 1)} disabled={widgets.at(-1)?.id === widget.id} class="rounded p-1.5 hover:bg-accent hover:text-foreground disabled:opacity-20" aria-label={`Move ${dashboardWidgetMeta[widget.id].label} later`}><ArrowDown class="h-3.5 w-3.5" /></button>
							</div>
							<div class="hidden opacity-50 transition-opacity hover:opacity-100 focus-within:opacity-100 xl:flex">
								<button type="button" onclick={() => moveCustomWidget(widget.id, -1, 0)} disabled={!position || position.column === 0} class="rounded p-1.5 hover:bg-accent hover:text-foreground disabled:opacity-20" aria-label={`Move ${dashboardWidgetMeta[widget.id].label} left`}><ArrowLeft class="h-3.5 w-3.5" /></button>
								<button type="button" onclick={() => moveCustomWidget(widget.id, 1, 0)} disabled={!position || position.column + position.columnSpan >= 12} class="rounded p-1.5 hover:bg-accent hover:text-foreground disabled:opacity-20" aria-label={`Move ${dashboardWidgetMeta[widget.id].label} right`}><ArrowRight class="h-3.5 w-3.5" /></button>
								<button type="button" onclick={() => moveCustomWidget(widget.id, 0, -1)} disabled={!position || position.row === 0} class="rounded p-1.5 hover:bg-accent hover:text-foreground disabled:opacity-20" aria-label={`Move ${dashboardWidgetMeta[widget.id].label} up`}><ArrowUp class="h-3.5 w-3.5" /></button>
								<button type="button" onclick={() => moveCustomWidget(widget.id, 0, 1)} class="rounded p-1.5 hover:bg-accent hover:text-foreground" aria-label={`Move ${dashboardWidgetMeta[widget.id].label} down`}><ArrowDown class="h-3.5 w-3.5" /></button>
								<span draggable={true} ondragstart={(event) => startWidgetDrag(event, widget.id)} ondragend={endWidgetDrag} class="inline-flex cursor-grab rounded p-1.5 hover:bg-accent hover:text-foreground active:cursor-grabbing" title="Drag to a grid position" aria-hidden="true"><GripVertical class="h-3.5 w-3.5" /></span>
							</div>
							<WidgetOptions {widget} onChange={(patch) => updateWidgetAppearance(widget.id, patch)} />
							<button
								type="button"
								onclick={() => void hideWidget(widget.id)}
								disabled={widgets.length <= 1}
								data-dashboard-remove={widget.id}
								class="rounded p-1.5 text-muted-foreground hover:bg-destructive/10 hover:text-destructive disabled:opacity-20"
								aria-label={`Remove ${dashboardWidgetMeta[widget.id].label} from this view`}
								title={widgets.length <= 1 ? 'At least one widget must remain visible' : 'Remove from this view'}
							><X class="h-3.5 w-3.5" /></button>
						</div>
					</div>
				{/if}
				{#if widget.id === 'alerts'}
					<AlertsWidget {alerts} loaded={alertsLoaded} {density} presentation={widget.presentation} />
				{:else if widget.id === 'system'}
					<SystemWidget {info} {health} {infoLoaded} {healthLoaded} {density} presentation={widget.presentation} />
				{:else if widget.id === 'service_health'}
					<HealthWidget kind="services" services={serviceHealth} freshness={serviceHealthFreshness} {density} />
				{:else if widget.id === 'container_health'}
					<HealthWidget kind="containers" containers={containerHealth} freshness={containerHealthFreshness} {density} />
				{:else if widget.id === 'compute'}
					<ComputeWidget {vms} {appsStatus} containers={containerHealth} {vmFreshness} containerFreshness={containerHealthFreshness} {density} />
				{:else if widget.id === 'clock'}
					<ClockWidget {info} {settings} loaded={clockDataLoaded} {density} />
				{:else if widget.id === 'schedule'}
					<ScheduleWidget entries={scheduleEntries} currentTime={info?.current_time} freshness={scheduleFreshness} {density} />
				{:else if widget.id === 'cpu_load' || widget.id === 'memory_usage' || widget.id === 'cpu_status' || widget.id === 'storage_summary'}
					<SummaryWidget kind={widget.id} {stats} {filesystems} {filesystemsLoaded} {density} />
				{:else if widget.id === 'operations'}
					<OperationsWidget operations={systemStatus?.operations ?? []} loaded={operationsLoaded} {density} presentation={widget.presentation} />
				{:else if widget.id === 'storage'}
					<StorageWidget {filesystems} loaded={filesystemsLoaded} usages={filesystemUsages} health={diskHealth} rates={diskRates} {density} />
				{:else if widget.id === 'history'}
					<HistoryWidget cpuSamples={cpuSamples} memorySamples={memorySamples} range={metricsRange} loading={historyLoading} width={widget.width} {density} presentation={widget.presentation} />
				{:else if widget.id === 'network' && stats}
					<NetworkWidget interfaces={stats.network} rates={networkRates} samples={networkSamples} {density} />
				{:else if widget.id === 'disk_io' && stats}
					<DiskIoWidget devices={stats.disk_io} rates={diskRates} samples={diskSamples} {density} />
				{:else}
					<Card><CardContent class="py-8 text-center text-sm text-muted-foreground">Widget data is unavailable.</CardContent></Card>
				{/if}
			</div>
		{/each}
	</div>
{/if}
</div>

<CustomizeDialog bind:open={customizeOpen} preferences={dashboardPrefs.value} onSave={(preferences) => void applyPreferences(preferences, true)} />

<style>
	@media (min-width: 80rem) {
		.dashboard-positioned {
			grid-column-start: var(--dashboard-column);
			grid-row-start: var(--dashboard-row);
		}
	}
</style>
