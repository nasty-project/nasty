// Mirrors engine Rust types

/** Error code emitted by `auth.webauthn.register.start` when the
 * caller has no fallback factor (no local password and no OIDC
 * link). Surfaced verbatim by the engine so the WebUI can match on
 * it without re-translating the user-facing message. Kept as a
 * substring check on the engine's full error string because the
 * engine returns a String, not a structured error code. */
export const WEBAUTHN_NO_FALLBACK_HINT = 'set a password or single sign-on';

/** One registered WebAuthn credential under the current user. Wire
 * shape returned by `auth.webauthn.list` and (on a single-row form)
 * `auth.webauthn.register.finish`. The credential_id is the
 * base64url stable identifier the WebUI passes back to
 * `auth.webauthn.delete` — labels aren't unique by intent, so we
 * key deletion off the cred id. */
export interface WebauthnCredentialSummary {
	label: string;
	created_at: number;
	credential_id: string;
}

/** Wire shape of `auth.webauthn.config`. Exposes the engine-pinned
 * RP ID so the WebUI can pre-check `window.location` before
 * attempting `navigator.credentials.create` — the browser refuses
 * to issue a credential when the origin can't satisfy the RP ID
 * (IP origins, mismatched hostnames, plain http://) and surfaces
 * the rejection as a cryptic error. */
export interface WebauthnConfigInfo {
	rp_id: string;
}

/** Wire shape of `auth.webauthn.register.start`. `creation_options`
 * is the spec-shaped JSON `PublicKeyCredentialCreationOptions` —
 * pass it through `@simplewebauthn/browser`'s `startRegistration`
 * helper which handles the base64url ↔ ArrayBuffer conversion.
 * The `registration_id` round-trips back to `register.finish` so
 * the engine can pair the browser's response with the matching
 * server-side `PasskeyRegistration` state. */
export interface WebauthnRegisterStart {
	registration_id: string;
	// The shape is webauthn-rs's `CreationChallengeResponse` — treat
	// as opaque on the WebUI side; only `@simplewebauthn/browser`
	// reads it.
	creation_options: unknown;
}

export interface SystemInfo {
	hostname: string;
	version: string;
	uptime_seconds: number;
	kernel: string;
	bcachefs_version: string;
	bcachefs_commit: string | null;
	bcachefs_pinned_ref: string | null;
	/** bcachefs-tools ref this NASty build ships with; the top-bar chip
	 * offers to switch the operator's pin to this when they differ. */
	bcachefs_recommended_ref: string | null;
	bcachefs_is_custom: boolean;
	timezone: string;
	ntp_synced: boolean;
}

export interface SystemHealth {
	status: string;
	services: ServiceStatus[];
}

/** A long-running array operation in progress (#528). */
export interface ActiveOperation {
	kind: string; // "evacuate" | "scrub" | "reconcile"
	fs: string;
	target?: string | null;
	progress_percent?: number | null;
	detail: string;
}

/** Aggregated system status for the sidebar band (#528). */
export interface SystemStatus {
	level: string; // "healthy" | "activity" | "critical"
	headline: string;
	operations: ActiveOperation[];
	critical_count: number;
	warning_count: number;
}

/** A controllable data operation for the Operations panel (#553), from
 * `system.operations.list`. Carries the action the UI can take. */
export interface Operation {
	kind: string; // "scrub" | "evacuate" | "reconcile" | "copygc"
	fs: string;
	target?: string | null;
	state: string; // "running" | "active" | "idle" | "paused"
	progress_percent?: number | null;
	detail: string;
	control: string; // "cancel" | "pause" | "resume" | "none"
}

/** One IOMMU group with its constituent PCI devices, returned by
 * `system.hardware.iommu`. The id is the kernel's group number;
 * devices are sorted by BDF. Empty list = IOMMU is off in BIOS. */
export interface IommuGroup {
	id: number;
	devices: PciDevice[];
}

/** One PCI device. Numeric IDs are 4-hex-digit strings. Names come
 * from `pci.ids` via `lspci` and may be null on bleeding-edge or
 * exotic hardware. `driver` is the currently bound kernel module
 * (`vfio-pci` = ready for passthrough). */
export interface PciDevice {
	bdf: string;
	vendor_id: string;
	device_id: string;
	class_id: string;
	vendor_name: string | null;
	device_name: string | null;
	class_name: string | null;
	driver: string | null;
}

/** Hardware overview, returned by `system.hardware.summary`. Server-
 * side cached for 60s; expect stale-by-up-to-a-minute data after
 * suspend/resume cycles, but the underlying physical hardware
 * doesn't change between boots so this is fine. */
export interface HardwareSummary {
	system: DmiSystem | null;
	bios: DmiBios | null;
	cpu: CpuSummary | null;
	memory: MemorySummary;
	usb: UsbDevice[];
	tpm: TpmInfo | null;
	secure_boot: SecureBootStatus;
}

/** Sourced from `bootctl status` on the engine. All fields degrade
 * to null when the box isn't UEFI, bootctl is missing, or its output
 * lacks the Secure Boot line — those failure modes surface as
 * `enabled: null` with a human-readable `note`, never as a missing
 * field. `unsupported = true` distinguishes "firmware can't do SB"
 * from "firmware can but operator hasn't enabled it", so the WebUI
 * doesn't nudge the operator toward a firmware setting that isn't
 * there. */
export interface SecureBootStatus {
	enabled: boolean | null;
	setup_mode: boolean | null;
	unsupported: boolean | null;
	measured_uki: boolean | null;
	note: string | null;
}

/** Wire shape of `system.secure_boot.enrollment.status`. Drives the
 * highly-experimental SB onboarding wizard on the Hardware page.
 * `phase.kind` is the discriminator; per-variant fields land
 * inline alongside it (serde flattens — `OverlayWritten` ships
 * `{kind: "overlay_written", overlay_at: 12345}`). */
export type SecureBootEnrollmentPhase =
	| { kind: 'not_started' }
	| { kind: 'overlay_written'; overlay_at: number }
	| { kind: 'post_enrollment'; detected_at: number; stale_tpm_bindings: string[] }
	| { kind: 'complete'; completed_at: number }
	| { kind: 'aborted'; aborted_at: number; reason: string };

export interface SecureBootEnrollmentState {
	phase: SecureBootEnrollmentPhase;
	initiated_by: string | null;
	/** Unix seconds when the wizard's Rebuild button last fired.
	 * `null` until the operator clicks Rebuild for the first time
	 * in a given ceremony. The Abort dialog uses this to decide
	 * whether "you'll need to rebuild once more to revert" applies
	 * (rebuild_triggered_at = number) or "abort is clean, nothing
	 * was applied" (null). Cleared on each Begin. */
	rebuild_triggered_at: number | null;
}

/** Live snapshot of the wizard-driven `nasty-rebuild` unit, queried
 * via systemctl on every status call. The wizard polls this every
 * few seconds while a rebuild is in flight; we don't try to
 * persist it because systemd is the source of truth and survives
 * engine restarts on its own. */
export interface SecureBootRebuildSnapshot {
	status: 'not_run' | 'running' | 'succeeded' | 'failed';
	exit_code: number | null;
	journal_tail: string[];
}

/** Combined response from `system.secure_boot.enrollment.status`.
 * `state` carries the persistent enrollment state; `rebuild` is
 * the live systemd-driven progress that the wizard renders next
 * to the per-phase step copy. */
export interface SecureBootEnrollmentStatusResponse extends SecureBootEnrollmentState {
	rebuild: SecureBootRebuildSnapshot;
}

/** Structured checklist returned by `system.secure_boot.readiness`.
 * Drives the Hardware-page panel that shows whether a box is ready
 * for the lanzaboote opt-in. `ready === true` means every check
 * passes and the (PR #2b) "Enable Secure Boot" affordance is
 * available; otherwise `blocker` names the obstacle. Each individual
 * field exposes the underlying signal so the UI can render an
 * itemised checklist with pass/fail/not-applicable per row. */
export interface SecureBootReadinessReport {
	ready: boolean;
	blocker: string | null;
	uefi_boot: boolean;
	sb_supported_by_firmware: boolean | null;
	sb_currently_off: boolean | null;
	tpm2_available: boolean;
	esp_free_bytes: number | null;
	esp_required_bytes: number;
	wrapper_has_lanzaboote_input: boolean | null;
	sbctl_keys_already_generated: boolean;
}

export interface TpmInfo {
	/** 1 = TPM 1.2 (incompatible with planned sealing), 2 = TPM 2.0. */
	version_major: number | null;
	/** `/dev/tpmrm0` is present — the resource-manager dev that tpm2-tools
	 * and any sealing path actually opens. False means a chip exists but
	 * isn't usable from userspace. */
	rm_available: boolean;
	/** 4-char ASCII manufacturer code reported by the chip via
	 * `TPM2_PT_MANUFACTURER`: IFX (Infineon), STM (STMicroelectronics),
	 * NTC (Nuvoton), IBM (swtpm), AMD (fTPM), etc. */
	manufacturer: string | null;
	/** Vendor's marketing model string from `TPM2_PT_VENDOR_STRING_1..4`,
	 * concatenated and trimmed. E.g. "SLB9665" for Infineon, "SW  TPM"
	 * for swtpm. */
	vendor_string: string | null;
}

export interface DmiSystem {
	manufacturer: string | null;
	product: string | null;
	version: string | null;
}

export interface DmiBios {
	vendor: string | null;
	version: string | null;
	release_date: string | null;
}

export interface CpuSummary {
	model: string | null;
	vendor: string | null;
	physical_cores: number;
	logical_cores: number;
	max_mhz: number | null;
}

export interface MemorySummary {
	total_bytes: number;
	slots_total: number;
	slots_used: number;
	ecc: boolean;
	dimms: DimmInfo[];
}

export interface DimmInfo {
	locator: string;
	size_bytes: number;
	mem_type: string | null;
	speed_mts: number | null;
	manufacturer: string | null;
	part_number: string | null;
}

export interface UsbDevice {
	bus: number;
	device: number;
	vendor_id: string;
	product_id: string;
	description: string;
}

/** One PCI device-class identifier — what `vfio-pci.ids=` consumes.
 * Granularity: vendor:device, not BDF, so the binding survives slot
 * moves. Caveat: marking a (vendor, device) pair claims **all**
 * matching devices. */
/** One RDMA device from /sys/class/infiniband (`system.rdma.status`). */
export interface RdmaDevice {
	name: string;
	/** "InfiniBand" | "Ethernet" (RoCE / soft-RoCE). */
	link_layer: string;
	netdevs: string[];
}

/** RDMA capability + opt-in state from `system.rdma.status`. */
export interface RdmaStatus {
	enabled: boolean;
	capable: boolean;
	devices: RdmaDevice[];
	ib_isert_available: boolean;
	nvmet_rdma_available: boolean;
	nfs_rdma_available: boolean;
	nfs_rdma_active: boolean;
	blocker?: string | null;
}

export interface PassthroughDeviceId {
	vendor: string;
	device: string;
}

/** One claimed PCI device — passthrough claims are per-BDF. */
export interface PassthroughEntry {
	address: string;
	vendor: string;
	device: string;
}

export interface PassthroughConfig {
	/** Per-device claims (authoritative). */
	devices: PassthroughEntry[];
	/** Legacy vendor:device mirror kept for engine-rollback compat. */
	ids: PassthroughDeviceId[];
}

/** VM guest-tools state from `system.guest_tools.status`. The QEMU
 * guest agent is always-on (free, reuses bundled qemu) and not
 * represented here; `enabled` governs the heavier VMware/Hyper-V
 * integrations toggled via `system.guest_tools.set`. */
export interface GuestToolsStatus {
	enabled: boolean;
	/** `systemd-detect-virt` token: vmware | microsoft | kvm | qemu | none | … */
	hypervisor: string;
	is_vm: boolean;
	/** idle | running | failed */
	rebuild_state: string;
	log_tail: string | null;
}

/** Aggregated view of everything that depends on a filesystem,
 * returned by `fs.dependents`. Powers the impact-preview dialog
 * before destructive operations like Lock — the WebUI lists these
 * so the user sees what will break before they confirm. Empty
 * arrays serialize as `[]` so consumers can render unconditionally. */
/** Per-subvolume version of FsDependents. Returned by `subvolume.list_dependents`
 * (batched: all subvolumes in one call). Powers the Usage column on the
 * Subvolumes page so the operator sees at a glance what an unsafe rm/destroy
 * would take with it — apps, VMs, shares, backup jobs that live on or are
 * backed by this subvolume. */
export interface SubvolumeDependents {
	filesystem: string;
	name: string;
	path: string;
	apps: string[];
	vms: string[];
	backup_jobs: string[];
	nfs_shares: string[];
	smb_shares: string[];
	iscsi_targets: string[];
	nvmeof_subsystems: string[];
}

export interface FsDependents {
	filesystem: string;
	mounted: boolean;
	subvolumes: string[];
	apps: string[];
	vms: string[];
	backup_jobs: string[];
	nfs_shares: string[];
	smb_shares: string[];
	iscsi_targets: string[];
	nvmeof_subsystems: string[];
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
	/** bcachefs's own per-device Rotational flag from the superblock —
	 * what it uses for SSD optimizations. Distinct from the hardware
	 * type; can disagree if mis-latched (bcachefs-tools #594). */
	rotational?: boolean | null;
	/** Cumulative read IO errors since fs creation (mounted pools only). */
	read_errors?: number | null;
	/** Cumulative write IO errors since fs creation. */
	write_errors?: number | null;
	/** Cumulative checksum errors since fs creation. */
	checksum_errors?: number | null;
	/** bcachefs member slot (the `Device N` index) — stable across reboots. */
	member_index?: number | null;
	/** Stable per-device bcachefs UUID (mounted pools only). */
	uuid?: string | null;
	/** True for a missing member: the superblock still lists it but the
	 * device is detached (pulled, dead, or offlined). `path` is the real
	 * /dev node when the dropped device is still named in /proc/mounts,
	 * else a synthetic `(missing dev-N)` placeholder. Re-attach via
	 * fs.device.online if the disk is back (#472), or remove it by
	 * member_index with force. */
	missing?: boolean | null;
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
	last_mount_error?: MountFailure | null;
}

export type MountFailureReason =
	| 'missing_device'
	| 'needs_unlock'
	| 'needs_check'
	| 'busy'
	| 'unknown';

export interface MissingDevice {
	path: string;
	member_index?: number | null;
	label?: string | null;
}

export interface MountFailure {
	attempted_at: number;
	reason: MountFailureReason;
	message: string;
	missing_devices: MissingDevice[];
	raw: string;
}

export interface TpmBindStatus {
	tpm_available: boolean;
	bound: boolean;
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

export type ScrubOutcome = 'ok' | 'errors' | 'failed' | 'cancelled';

export interface ScrubStatus {
	running: boolean;
	/** Unix seconds when the current run started; set while running. */
	started_at?: number | null;
	/** 0-100 progress of the in-flight scrub, parsed from the most
	 * recent `XX%` token in bcachefs's streaming output. Only set
	 * while `running`. */
	progress_percent?: number | null;
	/** Unix seconds when the most recent completed scrub finished. */
	last_run_at?: number | null;
	/** Duration of the most recent completed scrub, in seconds. */
	last_duration_secs?: number | null;
	last_outcome?: ScrubOutcome | null;
	/** Captured stdout+stderr from the most recent completed scrub
	 * (trailing 8 KiB), or a one-line note for engine-restart-during-
	 * scrub. */
	last_output?: string | null;
	/** Backward-compat one-line summary the legacy Diagnostics tab
	 * displays verbatim. New surfaces should prefer the typed fields. */
	raw: string;
}

export interface ReconcileStatus {
	raw: string;
	enabled: boolean;
}

export type FsckOutcome = 'clean' | 'errors' | 'failed';

export interface FsckStatus {
	running: boolean;
	/** Whether the in-flight (or most recent) run was a repair (`-y`) vs dry run (`-n`). */
	repair: boolean;
	started_at?: number | null;
	progress_percent?: number | null;
	last_run_at?: number | null;
	last_duration_secs?: number | null;
	last_repair?: boolean | null;
	last_outcome?: FsckOutcome | null;
	last_output?: string | null;
}

export interface BlockDevice {
	path: string;
	size_bytes: number;
	dev_type: string;
	mount_point: string | null;
	fs_type: string | null;
	/** Filesystem UUID from lsblk — for bcachefs members this is the
	 * *external* (whole-pool) UUID, matchable against `Filesystem.uuid`
	 * to tell an offline/former member from a foreign disk (#472). */
	fs_uuid?: string;
	in_use: boolean;
	rotational: boolean;
	/** "nvme" | "ssd" | "hdd" */
	device_class: string;
	/** Drive model from lsblk; missing on partitions and many virtual disks. */
	model?: string;
	/** Drive serial from lsblk; same caveat. */
	serial?: string;
	/** Drive vendor from lsblk (e.g. "ATA", "NVMe"). */
	vendor?: string;
	/** Transport bus from lsblk (e.g. "sata", "nvme", "usb"). */
	transport?: string;
	/** Stable identity a manual type override is anchored to (by-id /
	 * by-path / dev name). Absent on partitions and "free" entries (#552). */
	stable_id?: string;
	/** Durability of `stable_id`: "hardware" (by-id) | "slot" (by-path) |
	 * "volatile" (/dev name). */
	id_kind?: string;
	/** "detected" (from lsblk/sysfs) | "manual" (operator override). */
	type_source: string;
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
	/** Hard quota limit in bytes (filesystem subvolumes only). null = no limit. */
	quota_bytes: number | null;
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
	time_machine: boolean;
	time_machine_max_size_gib: number | null;
	enabled: boolean;
}

export interface SmbGroup {
	name: string;
	gid: number;
	members: string[];
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
	/** iSER (iSCSI over RDMA) portal. */
	iser?: boolean;
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
	/** Number of registered WebAuthn credentials. Defaults to 0 for
	 * compat with engines that pre-date the field. Drives the admin
	 * "Reset security keys" button visibility on the /users page. */
	webauthn_credential_count?: number;
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
	/** smartctl transport flag used to reach this drive (`megaraid,0`,
	 * `sat+megaraid,2`, `areca,3`). `undefined` for drives reachable via
	 * smartctl's default transport. Together with `device` it uniquely
	 * identifies a physical drive — multiple drives behind a RAID
	 * controller share the same path but have distinct transports. */
	transport?: string;
	ata_port?: string;
	controller_pci?: string;
	controller_name?: string;
	pcie_link?: PcieLink;
	model: string;
	serial: string;
	firmware: string;
	capacity_bytes: number;
	temperature_c: number | null;
	power_on_hours: number | null;
	health_passed: boolean;
	smart_status: string;
	/** true = spinning HDD, false = SSD, null/undefined = unknown (NVMe
	 * dumps carry no rotation rate). */
	rotational?: boolean | null;
	attributes: SmartAttribute[];
	nvme?: NvmeHealth;
	scsi?: ScsiHealth;
	ata?: AtaHealth;
}

/** PCIe link state for a storage controller, sourced from
 * `/sys/bus/pci/devices/<bdf>/{current,max}_link_{speed,width}`.
 * When `current_*` is below `max_*` the link has trained down — common
 * causes include PCIe ASPM power saving, broken bifurcation in a U.2
 * backplane, a flaky riser cable, or a slot wired narrower than
 * physically advertised. Speed strings are passed through verbatim
 * from sysfs (e.g. `"8.0 GT/s PCIe"`). */
export interface PcieLink {
	current_speed: string;
	max_speed: string;
	current_width: number;
	max_width: number;
}

/** ATA / SATA summary fields complementing the generic SMART attribute
 * table. Populated only on ATA drives smartctl could query natively. */
export interface AtaHealth {
	interface_speed_current?: string;
	interface_speed_max?: string;
	/** Endurance consumed as percentage (0 = new, 100 = nominal end of
	 * life). Mirrors `NvmeHealth.percentage_used`. Sourced from
	 * smartctl 7.5+'s top-level `endurance_used.current_percent`.
	 * `undefined` on spinners, very old SSDs without
	 * Media_Wearout_Indicator, and pre-7.5 smartctl. */
	endurance_used_percent?: number;
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

export interface NvmeHealth {
	critical_warning: number;
	available_spare_percent: number;
	available_spare_threshold_percent: number;
	percentage_used: number;
	data_units_read: number;
	data_units_written: number;
	host_reads: number;
	host_writes: number;
	controller_busy_minutes: number;
	power_cycles: number;
	unsafe_shutdowns: number;
	media_errors: number;
	num_err_log_entries: number;
	/** Human-readable status of the most recent error log entry (e.g.
	 * `"Invalid Field in Command"`). Only smartctl 7.4+ surfaces the
	 * actual table behind `num_err_log_entries`; older smartctl + drives
	 * with an empty log report `undefined`. */
	most_recent_error?: string;
	warning_temp_minutes: number;
	critical_comp_minutes: number;
	temperature_sensors_c: (number | null)[];
}

/** SCSI / SAS health information. Populated only on SAS / SCSI drives,
 * including SAS drives reached via `-d megaraid,N`. Field names trace
 * back to the SCSI Primary Commands / Block Commands standards so they
 * match what `smartctl -a` prints. */
export interface ScsiHealth {
	transport_protocol?: string;
	scsi_version?: string;
	/** Rotation rate in RPM. `0` = SSD; typical SAS spinners: 7200,
	 * 10500/10033, 15000. */
	rotation_rate?: number;
	form_factor?: string;
	logical_unit_id?: string;
	/** Drive-trip temperature — the controller's hard shutdown threshold. */
	drive_trip_temp_c?: number;
	year_of_manufacture?: string;
	week_of_manufacture?: string;
	/** Sectors moved to spare blocks since manufacture. Non-zero is
	 * normal on aging drives; rate of growth matters more than count. */
	grown_defect_list?: number;
	power_on_minutes_since_format?: number;
	start_stop_cycles?: number;
	start_stop_cycles_designed?: number;
	load_unload_cycles?: number;
	load_unload_cycles_designed?: number;
	read_errors: ScsiErrorCounters;
	write_errors: ScsiErrorCounters;
	verify_errors: ScsiErrorCounters;
	/** Most recent entry from the SCSI Self-Test rolling log. */
	last_self_test?: ScsiSelfTestEntry;
	self_test_count: number;
}

export interface ScsiErrorCounters {
	corrected_total: number;
	/** Non-zero values are the failure signal — drive has lost or
	 * returned bad data. Engine flips `health_passed` to false when
	 * any I/O type's uncorrected_total > 0. */
	uncorrected_total: number;
	gigabytes_processed: number;
}

export interface ScsiSelfTestEntry {
	code: string;
	result: string;
	passed: boolean;
	power_on_hours?: number;
	in_progress: boolean;
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

/** Returned by `firmware.constraints`. Today only Secure Boot is
 * tracked — the EFI-capsule shim fwupd uses to apply updates
 * doesn't work under enforcing SB (upstream lanzaboote#591), so
 * the Apply button gates on `sb_blocks_apply` and renders the
 * `sb_blocks_apply_reason` string verbatim in a tooltip / banner
 * (no client-side translation; the engine owns the copy). */
export interface FirmwareConstraints {
	sb_blocks_apply: boolean;
	sb_blocks_apply_reason: string;
}

export type ReleaseChannel = 'mild' | 'spicy' | 'nasty';

export interface UpdateInfo {
	current_version: string;
	latest_version: string | null;
	update_available: boolean | null;
	channel: ReleaseChannel;
	/** "success" | "failed" | null — result of the most recent upgrade-unit run. */
	last_attempt: string | null;
	/** Engine-side error message when the latest-version lookup failed (GH unreachable, rate-limited, …). */
	error: string | null;
	/** Snapshot of every tracked flake input — populated by both system.update.version and system.update.check. */
	inputs: VersionInputInfo[] | null;
}

export interface VersionInputInfo {
	name: string;
	url: string;
	rev: string | null;
	/**
	 * Human-meaningful ref string from flake.lock's
	 * `nodes[<name>].original.ref` — typically a tag like `v1.38.3`
	 * or a branch name like `main`. Prefer this for display over
	 * `rev` (which is just a 12-char SHA prefix) when present.
	 */
	tag?: string;
}

// ── Boot status (engine /api/boot_status) ─────────────────────
//
// Engine startup walks a fixed list of restoration phases (mount
// filesystems, restart protocols, restore VMs/apps, etc.). Each
// phase runs under a wall-clock budget; failures are logged but
// don't take the engine down. The WebUI polls /api/boot_status
// during connect so it can show a "NASty is starting up" overlay
// before READY and a "something didn't come up cleanly" banner
// after. See #299.
export type BootPhaseState = 'pending' | 'running' | 'ok' | 'failed';

export interface BootPhase {
	name: string;
	state: BootPhaseState;
	started_at_ms: number | null;
	finished_at_ms: number | null;
	duration_ms: number | null;
	error: string | null;
}

export type BootOverallState = 'booting' | 'ready' | 'ready_with_errors';

export interface BootStatus {
	overall: BootOverallState;
	phases: BootPhase[];
	process_started_at_unix: number;
	ready_at_ms: number | null;
}

export interface UpdateBuildDirConfig {
	/** Persisted pool root (e.g. `/fs/first`); null when unset. */
	path: string | null;
	/** Mounted bcachefs pools discovered live from `/proc/mounts`. */
	available_pools: string[];
	/** Where the sandbox will actually land (`<pool>/.nasty-nix-build`). */
	resolved: string | null;
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

export type TempUnit = 'celsius' | 'fahrenheit';

export interface OidcRoleMapping {
	group: string;
	role: string;
}

export interface OidcSettings {
	enabled: boolean;
	issuer_url: string | null;
	client_id: string | null;
	client_secret: string | null;
	redirect_uri: string | null;
	scopes: string[];
	groups_claim: string;
	role_mappings: OidcRoleMapping[];
	default_role: string | null;
	auto_provision: boolean;
}

export interface Settings {
	timezone: string;
	hostname: string | null;
	clock_24h: boolean;
	temp_unit: TempUnit;
	tls_domain: string | null;
	tls_acme_email: string | null;
	tls_acme_enabled: boolean;
	tls_challenge_type: 'tls-alpn' | 'http' | 'dns';
	tls_dns_provider: string | null;
	/** Blanked by the engine once sealed; send "<unchanged>" to keep the
	 * stored value (#442 follow-up). */
	tls_dns_credentials: string | null;
	/** Opaque systemd-creds ciphertext — presence means credentials are
	 * stored. Never sent back by the UI. */
	tls_dns_credentials_encrypted?: unknown;
	tls_acme_staging: boolean;
	tls_dns_resolver: string | null;
	tls_dns_propagation_wait: number | null;
	telemetry_enabled: boolean;
	oidc: OidcSettings;
}

export interface TailscaleStatus {
	enabled: boolean;
	daemon_running: boolean;
	connected: boolean;
	ip?: string;
	hostname?: string;
	version?: string;
	has_auth_key: boolean;
}

export type NutMode = 'local' | 'remote';

export interface NutConfig {
	mode: NutMode;
	driver: string;
	port: string;
	ups_name: string;
	description: string;
	remote_host: string;
	remote_port: number;
	remote_username: string;
	remote_password: string;
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

// ── Networking ─────────────────────────────────────────────

export type IpMethod = 'dhcp' | 'static' | 'slaac' | 'inherit' | 'disabled';

export interface IpConfig {
	method: IpMethod;
	addresses: string[];
	gateway: string | null;
}

export interface InterfaceConfig {
	name: string;
	enabled: boolean;
	ipv4: IpConfig;
	ipv6: IpConfig;
	mtu: number | null;
	/** SR-IOV: VFs to create on this PF (absent = leave alone). */
	sriov_num_vfs?: number | null;
	/** SR-IOV: per-VF properties, applied alongside the VF count. */
	vfs?: VfConfig[];
}

/** Per-VF properties on an SR-IOV PF (`ip link set <pf> vf <n> ...`). */
export interface VfConfig {
	index: number;
	/** 802.1Q VLAN (1–4094); absent = untagged. */
	vlan?: number | null;
	/** Administrative MAC. */
	mac?: string | null;
	/** VF trust (promiscuous mode / MAC changes from the guest). */
	trust?: boolean | null;
	/** Spoof checking. */
	spoof_check?: boolean | null;
}

export type BondMode = 'lacp' | 'active_backup' | 'balance_rr' | 'balance_xor';

export interface BondConfig {
	name: string;
	members: string[];
	mode: BondMode;
	ipv4: IpConfig;
	ipv6: IpConfig;
	mtu: number | null;
	/** When true (default for newly-created bonds), the bond's MAC is
	 * taken from the primary member's live MAC instead of letting NM
	 * generate a random one. Keeps DHCP servers handing out the same
	 * lease across the enslave step — important when one of the
	 * members is the management interface. */
	inherit_member_mac?: boolean;
}

export interface VlanConfig {
	parent: string;
	vlan_id: number;
	ipv4: IpConfig;
	ipv6: IpConfig;
	mtu: number | null;
}

export interface BridgeConfig {
	name: string;
	members: string[];
	ipv4: IpConfig;
	ipv6: IpConfig;
	mtu: number | null;
	stp?: boolean;
	forward_delay_s?: number | null;
	/** Same semantics as `BondConfig.inherit_member_mac`: when true,
	 * the bridge takes its MAC from the primary member instead of
	 * getting a kernel-random MAC at creation. */
	inherit_member_mac?: boolean;
}

export interface NetworkConfig {
	interfaces: InterfaceConfig[];
	dns: string[];
	bonds: BondConfig[];
	vlans: VlanConfig[];
	bridges: BridgeConfig[];
}

export interface LiveInterface {
	name: string;
	mac: string;
	up: boolean;
	speed_mbps: number | null;
	carrier: boolean;
	ipv4_addresses: string[];
	ipv6_addresses: string[];
	mtu: number;
	kind: string;
	/** SR-IOV PF: maximum VFs the device supports. */
	sriov_total_vfs?: number | null;
	/** SR-IOV PF: currently-created VF count. */
	sriov_num_vfs?: number | null;
	/** SR-IOV VF: parent PF's interface name. */
	vf_of?: string | null;
	/** SR-IOV VF: index within the parent. */
	vf_index?: number | null;
}

export interface NetworkState {
	config: NetworkConfig;
	interfaces: LiveInterface[];
	/** Iface the WebUI is currently reaching the engine through. Used to
	 * warn before submitting a change that would disconnect the user. */
	mgmt_iface?: string | null;
}

/** Optional fields the WebUI can include when submitting a network update.
 * Server-side flatten means a bare NetworkConfig is also accepted. */
export interface NetworkUpdateRequest extends NetworkConfig {
	/** Seconds the user has to confirm the change before it auto-rolls back.
	 * Omit to let the server pick (30s for risky changes, none for safe). */
	confirm_within_secs?: number;
}

/** Returned by `system.network.update`.  Rollback-related fields are
 * populated only when the server scheduled one; `apply_errors` is
 * populated when one or more NM connections failed to apply (other
 * connections in the same payload may have succeeded — the engine
 * treats this as a partial success rather than a whole-apply
 * failure, but surfaces the per-connection messages so the user
 * isn't lied to). */
export interface NetworkUpdateResponse {
	txn_id?: string | null;
	revert_at_unix?: number | null;
	risk_reason?: string | null;
	apply_errors?: NetworkApplyError[];
}

export interface NetworkApplyError {
	connection_id: string;
	message: string;
}

/** One pending rollback transaction, as returned by `system.network.pending`.
 * Used by the WebUI to recover the rollback banner after a reconnect — the
 * original session that initiated the change may have lost connectivity
 * (e.g. on an IP change), but the new session can pick the txn back up. */
export interface NetworkPendingTxn {
	txn_id: string;
	revert_at_unix: number;
	risk_reason: string;
}

export interface FirewallRule {
	service: string;
	ports: { port: number; transport: 'tcp' | 'udp'; source: string | null; iface: string | null }[];
	active: boolean;
}

export interface PublishedAppPort {
	app: string;
	host_port: number;
	container_port: number;
	transport: string;
}

export interface FirewallStatus {
	active: boolean;
	rules: FirewallRule[];
	restrictions: Record<string, string[]>;
	interface_restrictions: Record<string, string[]>;
	/** Host ports Docker apps publish. NOT governed by this firewall (Docker
	 * DNATs them past the input chain) — shown read-only for visibility. */
	published_app_ports?: PublishedAppPort[];
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

export type AlertMetric = 'fs_usage_percent' | 'cpu_load_percent' | 'memory_usage_percent' | 'disk_temperature' | 'smart_health' | 'swap_usage_percent' | 'bcachefs_degraded' | 'bcachefs_device_error' | 'bcachefs_device_state' | 'bcachefs_io_errors' | 'bcachefs_scrub_errors' | 'bcachefs_reconcile_stalled' | 'root_disk_free_gb' | 'boot_disk_free_mb' | 'kernel_errors';
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

// ── Backups ────────────────────────────────────────────────

export interface BackupProfile {
	id: string;
	name: string;
	enabled: boolean;
	sources: string[];
	target: BackupTarget;
	schedule: string | null;
	retention: RetentionPolicy;
	/** Repository password. Optional on the wire: the engine accepts it
	 * on input (operator creating / rotating) and redacts it to "***"
	 * (or omits entirely once an encrypted blob exists) on output.
	 * Treat any value other than what the operator just typed as
	 * informational — only send back when actually rotating. */
	password?: string;
	snapshot_before: boolean;
	repo_initialized: boolean;
	last_run: BackupRunResult | null;
	/** PEM-encoded CA certificate the operator wants trusted for this
	 * profile's target (typically a self-signed CA fronting their
	 * rest-server / MinIO). Engine writes it to disk and passes the
	 * path through to rustic_backend's `cacert` option. */
	trusted_cacert?: string;
}

export type BackupTarget =
	| { type: 'local'; path: string }
	| {
		type: 's3';
		endpoint: string;
		bucket: string;
		access_key: string;
		/** Optional on the wire — the engine accepts plaintext on
		 * input, redacts to "***" or omits on output (the encrypted
		 * blob is held server-side). Omit from the update payload to
		 * carry the existing secret forward. */
		secret_key?: string;
		region?: string | null;
	  }
	| { type: 'sftp'; host: string; user: string; path: string; port?: number | null }
	| {
		type: 'rest';
		url: string;
		/** HTTP basic-auth username; the rest-server requires auth as
		 * of #408. Empty / null for legacy unauthenticated servers. */
		username?: string | null;
		/** Password as the operator supplied it. Round-trips as `"***"`
		 * on output once the engine has sealed it via systemd-creds —
		 * same shape as `S3.secret_key` / `B2.account_key`. */
		password?: string | null;
	}
	| {
		type: 'b2';
		bucket: string;
		account_id: string;
		/** Optional on the wire — same carry-forward story as S3.secret_key. */
		account_key?: string;
	  };

export interface RetentionPolicy {
	keep_last: number | null;
	keep_daily: number | null;
	keep_weekly: number | null;
	keep_monthly: number | null;
	keep_yearly: number | null;
}

export interface BackupRunResult {
	timestamp: string;
	success: boolean;
	message: string;
	duration_secs: number;
	bytes_added: number | null;
	files_new: number | null;
	files_changed: number | null;
}

export interface BackupSnapshot {
	id: string;
	time: string;
	hostname: string;
	paths: string[];
	tags: string[];
}

export interface BackupStatus {
	running: boolean;
	profile_id: string | null;
	progress: string | null;
}

/** A long-running backup operation (init / run / check). Returned by
 * `backup.repo.init`, `backup.run`, `backup.repo.check`; polled via
 * `backup.jobs.get`/`backup.jobs.list`. The engine starts the work in
 * a background tokio task and the client watches the state transition
 * Pending → Running → Succeeded|Failed. Read-only on the client side. */
export type BackupJobKind = 'init_repo' | 'run_backup' | 'check_repo';
export type BackupJobState = 'pending' | 'running' | 'succeeded' | 'failed';

export interface BackupJob {
	id: string;
	profile_id: string;
	kind: BackupJobKind;
	state: BackupJobState;
	created_at: string;
	started_at?: string | null;
	finished_at?: string | null;
	progress?: string | null;
	/** Engine result payload on success. For `init_repo`/`check_repo`
	 * this is a status message string; for `run_backup` it's a
	 * `BackupRunResult` JSON object (bytes_added, files_new, …). */
	result?: unknown;
	error?: string | null;
}

/** Returned by `backup.secrets_status`. Tells the WebUI whether
 * `systemd-creds` is healthy on this host and (when it is) which
 * backend it picked. Drives the small status pill on the Backups
 * page so the operator can see at a glance whether their stored
 * backup passwords / cloud keys are encrypted at rest. */
export type SecretsBackend = 'tpm-and-host' | 'host-only';

export type SecretsStatus =
	| { status: 'available'; backend: SecretsBackend }
	| { status: 'unavailable'; reason: string };

// ── Notifications ──────────────────────────────────────────

export interface NotificationConfig {
	channels: NotificationChannel[];
}

export interface NotificationChannel {
	id: string;
	name: string;
	enabled: boolean;
	type: 'smtp' | 'telegram' | 'webhook' | 'ntfy' | 'signal';
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
	/** Optional HMAC-SHA256 signing key. When set, every webhook POST
	 * carries an `X-NASty-Signature: sha256=<hex>` header. Receivers
	 * verify by recomputing HMAC-SHA256 of the raw body with the same
	 * key — proves the request actually came from NASty. */
	secret?: string;
	// ntfy
	server_url?: string;
	topic?: string;
	token?: string;
	// Signal
	api_url?: string;
	from_number?: string;
	to_number?: string;
}

// ── Virtual Machines ────────────────────────────────────────

export interface VmDisk {
	path: string;
	/** Stable backing file for a block-subvolume disk; the engine
	 * re-resolves `path` from this on start since loop device numbers
	 * change across reboots (#592). Absent for image-file disks. */
	source?: string;
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

/** USB device pinned for passthrough. We identify by vendor:product
 * because USB enumeration order is not stable across reboots; the
 * tradeoff is that any device matching the pair attaches (plugging
 * two identical dongles passes both through). */
export interface UsbPassthrough {
	vendor_id: string;
	product_id: string;
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
	usb_devices?: UsbPassthrough[];
	/** CD-ROM ISO paths attached to the VM. First entry is the one that
	 * boots when `boot_order === 'cdrom'`; additional entries surface
	 * inside the guest as extra read-only CDs (Win11 + virtio-win is
	 * the canonical multi-ISO case). */
	cdroms: string[];
	/** Legacy single-ISO field — mirrors `cdroms[0]` on the engine side.
	 * New WebUI code reads `cdroms` instead. Kept here so cross-version
	 * state files don't trip TypeScript at the boundary. */
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
	/** SR-IOV virtual function (has a physfn parent). */
	virtual_function?: boolean;
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
	/** Real path behind the stable /appdata symlink (#436). */
	appdata_path?: string;
	/** Whether /appdata currently resolves to an existing directory. */
	appdata_ok?: boolean;
}

export interface PruneResult {
	images_removed: number;
	space_reclaimed_bytes: number;
}

/** A compose stack's NASty-managed startup config (#437). */
export interface ComposeStartupEntry {
	name: string;
	managed: boolean;
	order: number;
	delay_secs: number;
}

export interface App {
	name: string;
	image: string;
	status: string;
	created: string;
	kind: string; // "simple" or "compose"
	containers?: AppContainer[];
	ports?: MappedPort[];
	/** True if deployed with allow_unsafe (elevated privileges). */
	unsafe_mode?: boolean;
	/** Human-readable reason the reverse-proxy ingress was disabled at
	 * install time (engine post-install probe detected absolute root-path
	 * assets that the path-prefix proxy can't route). When present, the
	 * apps list shows a "Direct port only" badge with this as a tooltip
	 * and hides the "Open" link. */
	proxy_disabled_reason?: string | null;
	/** NASty-managed Docker network the app is attached to, if any. */
	network?: string | null;
	/** The app's IP on that network, when known (LAN-IP apps). */
	network_ip?: string | null;
}

export interface AppContainer {
	name: string;
	container_id: string;
	image: string;
	status: string;
}

export interface AppStats {
	name: string;
	cpu_percent: number;
	memory_bytes: number;
	memory_limit_bytes: number;
	net_rx_bytes: number;
	net_tx_bytes: number;
	block_read_bytes: number;
	block_write_bytes: number;
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
	/** `is_image_default: true` means the row's value matches the image's
	 * own `Config.Env` default for that key — the user didn't set it.
	 * Edit greys these rows out with an "Override" button so the user
	 * sees what the image provides without being misled into thinking
	 * they own it. Only present when the engine recognised the default. */
	env: { name: string; value: string; is_image_default?: boolean }[];
	volumes: { name: string; mount_path: string; host_path: string }[];
	cpu_limit: string | null;
	memory_limit: string | null;
	/** True if app was originally deployed with allow_unsafe. */
	allow_unsafe?: boolean;
	/** Managed network the app is attached to (round-tripped on Edit). */
	network?: string | null;
	/** Static IP requested at install (round-tripped on Edit). */
	static_ip?: string | null;
	/** Subdomain-ingress hostname, if any (round-tripped on Edit so saving
	 * other fields doesn't drop the ingress). */
	subdomain?: string | null;
}

/** A NASty-managed Docker network spec (apps.networks.create payload). */
export interface ManagedNetwork {
	name: string;
	driver: string; // "bridge" | "macvlan" | "ipvlan"
	parent?: string | null;
	subnet?: string | null;
	gateway?: string | null;
	ip_range?: string | null;
	vlan?: number | null;
	host_shim?: boolean;
	/** Host's address on the container subnet (CIDR) — required with host_shim. */
	shim_ip?: string | null;
}

/** apps.networks.list row: spec + live-state annotations. */
export interface NetworkSummary extends ManagedNetwork {
	exists: boolean;
	managed: boolean;
	attached_apps: string[];
}

export interface ImageInspectResult {
	ports: { name: string; container_port: number; host_port: number | null; protocol: string }[];
	volumes: { name: string; mount_path: string; host_path: string }[];
	user?: string | null;
	/** Curated recipe for serving this image under /apps/<name>/. When
	 * present, the install form offers an "Apply" button that appends
	 * the recipe's env entries (with `{name}`/`{host}`/`{scheme}`
	 * placeholders substituted) — see SubPathRecipe in nasty-apps. */
	subpath_recipe?: SubPathRecipe | null;
}

export interface SubPathRecipe {
	display_name: string;
	env: { name: string; value: string }[];
}

export interface AppIngress {
	name: string;
	host_port: number;
	path: string;
	/** Fully-qualified hostname when the app is served in subdomain mode
	 * (Caddy matches the route by host instead of path prefix). Set via
	 * apps.ingress.set. When present, the Open button links to
	 * https://<subdomain>/ rather than /apps/<name>/. Omitted/null =
	 * path-prefix mode, the historical default. */
	subdomain?: string | null;
}

/** One row in the Ingress overview page — every route Caddy is serving,
 * engine-owned or static. Returned by `apps.caddy.routes`. Read-only:
 * engine-owned rows are edited through the Apps page (apps.ingress.set);
 * static rows are baked into the Caddyfile via NixOS. */
export interface CaddyRouteSummary {
	/** "host" | "path" | "catch_all" | "other" */
	match_kind: string;
	match_value: string;
	upstream: string | null;
	/** "reverse_proxy" | "file_server" | "static_response" | "rewrite" | "other" | "unknown" */
	handler_kind: string;
	/** "engine-app" | "static" */
	source: string;
	app_name: string | null;
	/** Caddy server name ("srv0" | "srv1" | …) for grouping. */
	server: string;
	/** On-disk certificate Caddy currently serves for this route's host.
	 * Populated only for host-match rows that have a cert in Caddy's data
	 * directory. Absent = no cert yet (pending / Caddy hasn't issued one)
	 * or not applicable (path / catch-all routes). */
	cert?: HostCert | null;
}

/** Subset of the per-host certificate Caddy serves, surfaced on the
 * Ingress overview row for the corresponding host-match route. */
export interface HostCert {
	issuer: string | null;
	issued: string | null;
	expires: string | null;
	/** Days until expiry from now; negative = expired. Used by the WebUI
	 * to colour the badge — red ≤ 7, amber ≤ 30, green otherwise. */
	expires_in_days: number | null;
	path: string;
}
