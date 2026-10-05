/** Keep the dropdown choice independent of the custom code being edited. */
export class TlsDnsProviderForm {
	selection = $state('');
	customCode = $state('');

	get provider(): string {
		return this.selection === 'other' ? this.customCode.trim() : this.selection;
	}

	load(savedProvider: string, knownProviders: readonly { code: string }[]): void {
		const provider = savedProvider.trim();
		const custom = !!provider && !knownProviders.some(p => p.code === provider);
		this.selection = custom ? 'other' : provider;
		this.customCode = custom ? provider : '';
	}

	validationError(acmeEnabled: boolean, challengeType: string): string | null {
		if (!acmeEnabled || challengeType !== 'dns' || this.provider) return null;
		return this.selection === 'other' ? 'Enter a DNS provider code.' : 'Select a DNS provider.';
	}
}
