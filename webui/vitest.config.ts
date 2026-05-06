import { defineConfig } from 'vitest/config';

// Kept separate from vite.config.ts so vitest's bundled vite copy doesn't
// clash with the SvelteKit plugin's plugin types under svelte-check.
export default defineConfig({
	test: {
		include: ['src/**/*.{test,spec}.ts'],
		setupFiles: ['./vitest.setup.ts']
	}
});
