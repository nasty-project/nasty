import type { FsDependents } from './types';

/** Build a human-readable summary of which downstream entities depend
 * on a filesystem, suitable for a confirmation dialog body. Returns
 * `null` when nothing depends on the FS (caller should use the
 * lighter "are you sure?" prompt instead). Pure — no DOM, easy
 * to unit-test. */
export function summarizeDependents(deps: FsDependents): string | null {
	const groups: { label: string; items: string[] }[] = [
		{ label: 'subvolume', items: deps.subvolumes },
		{ label: 'app', items: deps.apps },
		{ label: 'VM', items: deps.vms },
		{ label: 'backup job', items: deps.backup_jobs },
		{ label: 'NFS share', items: deps.nfs_shares },
		{ label: 'SMB share', items: deps.smb_shares },
		{ label: 'iSCSI target', items: deps.iscsi_targets },
		{ label: 'NVMe-oF subsystem', items: deps.nvmeof_subsystems },
	];
	const non_empty = groups.filter((g) => g.items.length > 0);
	if (non_empty.length === 0) return null;

	const lines = non_empty.map((g) => `• ${formatLine(g.label, g.items)}`);
	return lines.join('\n');
}

/** Format one group as a single line: `• 2 apps (jellyfin, transmission)`.
 * Pluralization is naive — singular/plural by trailing `s` — and we
 * cap the inline name list at 5 to keep the dialog from growing
 * unbounded on busy boxes; the rest get a `… +N more` summary. */
function formatLine(label: string, items: string[]): string {
	const noun = items.length === 1 ? label : pluralize(label);
	const max = 5;
	const shown = items.slice(0, max);
	const more = items.length - shown.length;
	const tail = more > 0 ? `${shown.join(', ')}, … +${more} more` : shown.join(', ');
	return `${items.length} ${noun} (${tail})`;
}

function pluralize(label: string): string {
	// "subvolume" → "subvolumes", "iSCSI target" → "iSCSI targets",
	// "VM" → "VMs". The labels in `summarizeDependents` are
	// hand-curated to make this trivial.
	return `${label}s`;
}
