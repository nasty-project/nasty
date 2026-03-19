// Mirrors engine Rust types

export interface SystemInfo {
	hostname: string;
	version: string;
	uptime_seconds: number;
	kernel: string;
	bcachefs_version: string;
	bcachefs_commit: string | null;
	bcachefs_pinned_ref: string | null;
	bcachefs_is_custom: boolean;
	timezone: string;
	ntp_synced: boolean;
}

export interface SystemHealth {
	status: string;
	services: ServiceStatus[];
}

export interface ServiceStatus {
	name: string;
	running: boolean;
}

export interface PoolDevice {
	path: string;
	/** Hierarchical label for tiering (e.g. "ssd.fast", "hdd.archive") */
	label: string | null;
	/** Durability: 0 = cache, 1 = normal, 2 = hardware RAID */
	durability: number | null;
	/** Device state: rw, ro, evacuating, spare */
	state: string | null;
	/** Data types allowed on this device (e.g. "journal,btree,user") */
	data_allowed: string | null;
	/** Data types currently present on this device */
	has_data: string | null;
	/** Whether TRIM/discard is enabled */
	discard: boolean | null;
}

export type DeviceState = 'rw' | 'ro' | 'failed' | 'spare';

export interface Pool {
	name: string;
	uuid: string;
	devices: PoolDevice[];
	mount_point: string | null;
	mounted: boolean;
	total_bytes: number;
	used_bytes: number;
	available_bytes: number;
	options: PoolOptions;
}

export interface PoolOptions {
	compression: string | null;
	background_compression: string | null;
	data_replicas: number | null;
	metadata_replicas: number | null;
	data_checksum: string | null;
	metadata_checksum: string | null;
	foreground_target: string | null;
	background_target: string | null;
	promote_target: string | null;
	metadata_target: string | null;
	erasure_code: boolean | null;
	encrypted: boolean | null;
	error_action: string | null;
}

export interface FsUsage {
	raw: string;
	devices: FsDeviceUsage[];
	data_bytes: number;
	metadata_bytes: number;
	reserved_bytes: number;
}

export interface FsDeviceUsage {
	path: string;
	used_bytes: number;
	free_bytes: number;
	total_bytes: number;
}

export interface ScrubStatus {
	running: boolean;
	raw: string;
}

export interface ReconcileStatus {
	raw: string;
}

export interface BlockDevice {
	path: string;
	size_bytes: number;
	dev_type: string;
	mount_point: string | null;
	fs_type: string | null;
	in_use: boolean;
	rotational: boolean;
	/** "nvme" | "ssd" | "hdd" */
	device_class: string;
}

export type TieringProfileId = 'single' | 'write_cache' | 'full_tiering' | 'none' | 'manual';

export interface TieringProfile {
	id: TieringProfileId;
	name: string;
	tagline: string;
	description: string;
	available: boolean;
	recommended: boolean;
	foreground_target: string | null;
	metadata_target: string | null;
	background_target: string | null;
	promote_target: string | null;
	/** Maps device path → label to assign */
	device_labels: Record<string, string>;
}

export type SubvolumeType = 'filesystem' | 'block';

export interface Subvolume {
	name: string;
	pool: string;
	subvolume_type: SubvolumeType;
	path: string;
	used_bytes: number | null;
	compression: string | null;
	comments: string | null;
	volsize_bytes: number | null;
	block_device: string | null;
	snapshots: string[];
	owner: string | null;
}

export interface Snapshot {
	name: string;
	subvolume: string;
	pool: string;
	path: string;
	read_only: boolean;
}

export interface NfsShare {
	id: string;
	path: string;
	comment: string | null;
	clients: NfsClient[];
	enabled: boolean;
}

export interface NfsClient {
	host: string;
	options: string;
}

export interface SmbShare {
	id: string;
	name: string;
	path: string;
	comment: string | null;
	read_only: boolean;
	browseable: boolean;
	guest_ok: boolean;
	valid_users: string[];
	extra_params: Record<string, string>;
	enabled: boolean;
}

export interface IscsiTarget {
	id: string;
	iqn: string;
	alias: string | null;
	portals: Portal[];
	luns: Lun[];
	acls: Acl[];
	enabled: boolean;
}

export interface Portal {
	ip: string;
	port: number;
}

export interface Lun {
	lun_id: number;
	backstore_path: string;
	backstore_name: string;
	backstore_type: string;
	size_bytes: number | null;
}

export interface Acl {
	initiator_iqn: string;
	userid: string | null;
	password: string | null;
}

export interface NvmeofSubsystem {
	id: string;
	nqn: string;
	namespaces: Namespace[];
	ports: NvmeofPort[];
	allowed_hosts: string[];
	allow_any_host: boolean;
	enabled: boolean;
}

export interface Namespace {
	nsid: number;
	device_path: string;
	enabled: boolean;
}

export interface NvmeofPort {
	port_id: number;
	transport: string;
	addr: string;
	service_id: string;
	addr_family: string;
}

export interface UserInfo {
	username: string;
	role: 'admin' | 'readonly' | 'operator';
}

export interface ApiTokenInfo {
	id: string;
	name: string;
	role: 'admin' | 'readonly' | 'operator';
	created_at: number;
	pool: string | null;
	expires_at: number | null;
}

export interface ApiTokenCreated extends ApiTokenInfo {
	token: string;
}

export interface SystemStats {
	cpu: CpuStats;
	memory: MemoryStats;
	network: NetIfStats[];
	disk_io: DiskIoStats[];
}

export interface DiskIoStats {
	name: string;
	read_bytes: number;
	write_bytes: number;
	read_ios: number;
	write_ios: number;
	io_in_progress: number;
}

export interface CpuStats {
	count: number;
	load_1: number;
	load_5: number;
	load_15: number;
}

export interface MemoryStats {
	total_bytes: number;
	used_bytes: number;
	available_bytes: number;
	swap_total_bytes: number;
	swap_used_bytes: number;
}

export interface NetIfStats {
	name: string;
	rx_bytes: number;
	tx_bytes: number;
	rx_packets: number;
	tx_packets: number;
	speed_mbps: number | null;
	up: boolean;
	addresses: string[];
}

export interface DiskHealth {
	device: string;
	model: string;
	serial: string;
	firmware: string;
	capacity_bytes: number;
	temperature_c: number | null;
	power_on_hours: number | null;
	health_passed: boolean;
	smart_status: string;
	attributes: SmartAttribute[];
}

export interface SmartAttribute {
	id: number;
	name: string;
	value: number;
	worst: number;
	threshold: number;
	raw_value: number;
	failing: boolean;
}

export interface UpdateInfo {
	current_version: string;
	latest_version: string | null;
	update_available: boolean | null;
}

export interface BcachefsToolsInfo {
	pinned_ref: string | null;
	pinned_rev: string | null;
	running_version: string;
	is_custom: boolean;
	is_custom_running: boolean;
	default_ref: string;
	kernel_rust: boolean | null;
	debug_symbols: boolean;
	debug_checks: boolean;
	debug_checks_running: boolean;
}

export interface UpdateStatus {
	/** "idle", "running", "success", "failed" */
	state: string;
	log: string;
	/** True when the activated system has a different kernel than the booted one */
	reboot_required: boolean;
	/** True when the webui store path changed during this update (browser reload needed) */
	webui_changed: boolean;
}

export interface IoSample {
	ts: number;
	in_rate: number;
	out_rate: number;
}

export interface ResourceHistory {
	name: string;
	samples: IoSample[];
}

export interface ProtocolStatus {
	name: string;
	display_name: string;
	enabled: boolean;
	running: boolean;
	system_service: boolean;
}

export interface Settings {
	timezone: string;
	hostname: string | null;
	clock_24h: boolean;
}

export interface NetworkConfig {
	dhcp: boolean;
	interface: string;
	address: string | null;
	prefix_length: number | null;
	gateway: string | null;
	nameservers: string[];
	live_addresses: string[];
	live_gateway: string | null;
}

export interface AlertRule {
	id: string;
	name: string;
	enabled: boolean;
	metric: AlertMetric;
	condition: AlertCondition;
	threshold: number;
	severity: AlertSeverity;
}

export type AlertMetric = 'pool_usage_percent' | 'cpu_load_percent' | 'memory_usage_percent' | 'disk_temperature' | 'smart_health' | 'swap_usage_percent';
export type AlertCondition = 'above' | 'below' | 'equals';
export type AlertSeverity = 'warning' | 'critical';

export interface ActiveAlert {
	rule_id: string;
	rule_name: string;
	severity: AlertSeverity;
	metric: AlertMetric;
	message: string;
	current_value: number;
	threshold: number;
	source: string;
}
