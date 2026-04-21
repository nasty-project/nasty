<script lang="ts">
	import { onMount, onDestroy } from 'svelte';
	import { EditorView, keymap, lineNumbers, highlightActiveLine, drawSelection } from '@codemirror/view';
	import { EditorState } from '@codemirror/state';
	import { oneDark } from '@codemirror/theme-one-dark';
	import { yaml } from '@codemirror/lang-yaml';
	import { json } from '@codemirror/lang-json';
	import { linter, type Diagnostic } from '@codemirror/lint';
	import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';

	interface Props {
		value: string;
		lang?: 'yaml' | 'json';
		placeholder?: string;
		readonly?: boolean;
		class?: string;
		/** Line numbers to mark as errors (1-based) */
		errorLines?: number[];
		oninput?: (value: string) => void;
	}

	let {
		value = $bindable(''),
		lang = 'yaml',
		placeholder = '',
		readonly = false,
		class: className = '',
		errorLines = [],
		oninput,
	}: Props = $props();

	let container: HTMLDivElement;
	let view: EditorView | null = null;
	let skipUpdate = false;

	// eslint-disable-next-line -- lang is intentionally captured once (doesn't change after mount)
	let langExtension: ReturnType<typeof yaml> | ReturnType<typeof json>;
	$effect(() => { langExtension = lang === 'json' ? json() : yaml(); });

	// Error line linter — marks specific lines
	function errorLineLinter() {
		return linter((view) => {
			const diagnostics: Diagnostic[] = [];
			for (const lineNo of errorLines) {
				if (lineNo < 1 || lineNo > view.state.doc.lines) continue;
				const line = view.state.doc.line(lineNo);
				diagnostics.push({
					from: line.from,
					to: line.to,
					severity: 'error',
					message: '',
				});
			}
			return diagnostics;
		});
	}

	onMount(() => {
		const extensions = [
			lineNumbers(),
			highlightActiveLine(),
			drawSelection(),
			history(),
			keymap.of([...defaultKeymap, ...historyKeymap]),
			langExtension,
			oneDark,
			EditorView.theme({
				'&': {
					fontSize: '13px',
					height: '100%',
				},
				'.cm-content': {
					fontFamily: "'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace",
					padding: '8px 0',
				},
				'.cm-gutters': {
					backgroundColor: 'transparent',
					borderRight: '1px solid hsl(var(--border))',
				},
				'.cm-scroller': {
					overflow: 'auto',
				},
				'&.cm-focused': {
					outline: 'none',
				},
				'.cm-line': {
					padding: '0 8px',
				},
			}),
			EditorView.updateListener.of((update) => {
				if (update.docChanged && !skipUpdate) {
					value = update.state.doc.toString();
					oninput?.(value);
				}
			}),
			errorLineLinter(),
		];

		if (readonly) {
			extensions.push(EditorState.readOnly.of(true));
		}

		if (placeholder) {
			extensions.push(EditorView.contentAttributes.of({ 'aria-placeholder': placeholder }));
		}

		view = new EditorView({
			state: EditorState.create({
				doc: value,
				extensions,
			}),
			parent: container,
		});
	});

	onDestroy(() => {
		view?.destroy();
	});

	// Sync external value changes into the editor
	$effect(() => {
		if (view && value !== view.state.doc.toString()) {
			skipUpdate = true;
			view.dispatch({
				changes: { from: 0, to: view.state.doc.length, insert: value },
			});
			skipUpdate = false;
		}
	});

	// Re-lint when errorLines change
	$effect(() => {
		if (view && errorLines) {
			// Force re-lint by reconfiguring
			view.dispatch({});
		}
	});
</script>

<div
	bind:this={container}
	class="overflow-hidden rounded-md border border-input {className}"
></div>
