import { describe, expect, it } from 'vitest';
import { render } from 'svelte/server';
import MaintenanceReboot from './MaintenanceReboot.svelte';

describe('maintenance reboot waiting screen', () => {
	it('does not claim readiness before the landing page becomes available', () => {
		const { body } = render(MaintenanceReboot);
		expect(body).toContain('Rebooting into storage maintenance');
		expect(body).toContain('Maintenance reboot requested.');
		expect(body).toContain('No repairs run automatically.');
		expect(body).toContain('sudo nasty-maintenance exit');
		expect(body).not.toContain('SSH service ready');
	});
});
