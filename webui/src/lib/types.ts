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
	memory_bytes?: number;
	cpu_seconds?: number;
	uptime_seconds?: number;
	pid?: number;
}

export interface FilesystemDevice {
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

export interface Filesystem {
	name: string;
	uuid: string;
	devices: FilesystemDevice[];
	mount_point: string | null;
	mounted: boolean;
	total_bytes: number;
	used_bytes: number;
	available_bytes: number;
	options: FilesystemOptions;
}

export interface FilesystemOptions {
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
	locked: boolean | null;
	key_stored: boolean | null;
	error_action: string | null;
	version_upgrade: string | null;
	degraded: boolean | null;
	verbose: boolean | null;
	fsck: boolean | null;
	journal_flush_disabled: boolean | null;
	journal_flush_delay: number | null;
	io_scheduler: string | null;
	move_ios_in_flight: number | null;
	move_bytes_in_flight: string | null;
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
	enabled: boolean;
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
	filesystem: string;
	subvolume_type: SubvolumeType;
	path: string;
	used_bytes: number | null;
	compression: string | null;
	comments: string | null;
	volsize_bytes: number | null;
	block_device: string | null;
	snapshots: string[];
	owner: string | null;
	properties: Record<string, string>;
	parent: string | null;
	direct_io: boolean;
	bcachefs_options?: Record<string, string>;
}

export interface Snapshot {
	name: string;
	subvolume: string;
	filesystem: string;
	path: string;
	read_only: boolean;
	parent: string | null;
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
	filesystem: string | null;
	expires_at: number | null;
	allowed_ips: string[];
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
	temp_c: number | null;
	freq_mhz: number | null;
	governor: string | null;
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
	ata_port?: string;
	controller_pci?: string;
	controller_name?: string;
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

export interface FirmwareDevice {
	name: string;
	device_id: string;
	version: string;
	vendor: string;
	update_available: boolean;
	update_version?: string;
	update_description?: string;
}

export interface FirmwareUpdateResult {
	device_name: string;
	success: boolean;
	message: string;
	reboot_required: boolean;
}

export type ReleaseChannel = 'mild' | 'spicy' | 'nasty';

export interface UpdateInfo {
	current_version: string;
	latest_version: string | null;
	update_available: boolean | null;
	channel: ReleaseChannel;
}

export interface VersionInputInfo {
	name: string;
	url: string;
	rev: string | null;
}

export interface VersionInfo {
	inputs: VersionInputInfo[];
}

export interface VersionTaggedReleaseStatus {
	current_url: string;
	latest_tag: string;
	latest_url: string;
	current_is_latest_standard_url: boolean;
}

export interface Generation {
	generation: number;
	date: string;
	nixos_version: string;
	kernel_version: string;
	nasty_version: string | null;
	current: boolean;
	booted: boolean;
	label: string | null;
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
	tls_domain: string | null;
	tls_acme_email: string | null;
	tls_acme_enabled: boolean;
	tls_challenge_type: 'tls-alpn' | 'dns';
	tls_dns_provider: string | null;
	tls_dns_credentials: string | null;
	telemetry_enabled: boolean;
}

export interface NutConfig {
	driver: string;
	port: string;
	ups_name: string;
	description: string;
	shutdown_on_battery_percent: number;
	shutdown_on_battery_seconds: number;
	shutdown_command: string;
}

export interface UpsStatus {
	status: string;
	battery_charge: number | null;
	battery_runtime: number | null;
	input_voltage: number | null;
	output_voltage: number | null;
	ups_load: number | null;
	ups_model: string | null;
	ups_serial: string | null;
	available: boolean;
	raw: Record<string, string>;
}

export interface TuningConfig {
	nfs_threads: number;
	nfs_lease_time: number;
	nfs_grace_time: number;
	smb_max_connections: number;
	smb_deadtime: number;
	smb_socket_options: string;
	iscsi_default_cmdsn_depth: number;
	iscsi_login_timeout: number;
	vm_dirty_ratio: number;
	vm_dirty_background_ratio: number;
	vm_dirty_expire_centisecs: number;
	vm_dirty_writeback_centisecs: number;
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

export type AlertMetric = 'fs_usage_percent' | 'cpu_load_percent' | 'memory_usage_percent' | 'disk_temperature' | 'smart_health' | 'swap_usage_percent' | 'bcachefs_degraded' | 'bcachefs_device_error' | 'bcachefs_device_state' | 'bcachefs_io_errors' | 'bcachefs_scrub_errors' | 'bcachefs_reconcile_stalled' | 'kernel_errors';
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

// ── Notifications ──────────────────────────────────────────

export interface NotificationConfig {
	channels: NotificationChannel[];
}

export interface NotificationChannel {
	id: string;
	name: string;
	enabled: boolean;
	type: 'smtp' | 'telegram' | 'webhook' | 'ntfy';
	// SMTP
	host?: string;
	port?: number;
	username?: string;
	password?: string;
	from?: string;
	to?: string;
	tls?: boolean;
	// Telegram
	bot_token?: string;
	chat_id?: string;
	// Webhook
	url?: string;
	headers?: Record<string, string>;
	// ntfy
	server_url?: string;
	topic?: string;
	token?: string;
}

// ── Virtual Machines ────────────────────────────────────────

export interface VmDisk {
	path: string;
	interface: string;
	readonly: boolean;
	cache?: string;
	aio?: string;
	discard?: string;
	iops_rd?: number;
	iops_wr?: number;
}

export interface VmNetwork {
	mode: string;
	bridge?: string;
	mac?: string;
}

export interface PassthroughDevice {
	address: string;
	label?: string;
}

export interface VmConfig {
	id: string;
	name: string;
	cpus: number;
	memory_mib: number;
	disks: VmDisk[];
	networks: VmNetwork[];
	passthrough_devices: PassthroughDevice[];
	boot_iso?: string;
	boot_order: string;
	uefi: boolean;
	description?: string;
	autostart: boolean;
	cpu_model?: string;
	machine_type?: string;
	vga?: string;
	extra_args?: string[];
}

export interface VmStatus extends VmConfig {
	running: boolean;
	pid?: number;
	vnc_port?: number;
}

export interface VmCapabilities {
	kvm_available: boolean;
	uefi_available: boolean;
	arch: string;
	passthrough_devices: PciDevice[];
}

export interface PciDevice {
	address: string;
	vendor_device: string;
	description: string;
	iommu_group: number;
	bound_to_vfio: boolean;
}

// ── Apps ────────────────────────────────────────────────────

export interface AppsStatus {
	enabled: boolean;
	running: boolean;
	app_count: number;
	memory_bytes?: number;
	storage_path?: string;
	storage_ok: boolean;
	docker_version?: string;
	disk_usage_bytes?: number;
}

export interface PruneResult {
	images_removed: number;
	space_reclaimed_bytes: number;
}

export interface App {
	name: string;
	image: string;
	status: string;
	created: string;
	kind: string; // "simple" or "compose"
	containers?: AppContainer[];
	ports?: MappedPort[];
}

export interface AppContainer {
	name: string;
	container_id: string;
	image: string;
	status: string;
}

export interface MappedPort {
	host_port: number;
	container_port: number;
	protocol: string;
}

export interface AppConfig {
	name: string;
	image: string;
	ports: { name: string; container_port: number; host_port: number | null; protocol: string }[];
	env: { name: string; value: string }[];
	volumes: { name: string; mount_path: string; host_path: string }[];
	cpu_limit: string | null;
	memory_limit: string | null;
}

export interface ImageInspectResult {
	ports: { name: string; container_port: number; host_port: number | null; protocol: string }[];
}

export interface AppIngress {
	name: string;
	host_port: number;
	path: string;
}
