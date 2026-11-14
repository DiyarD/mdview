// Entry point for the vendored CodeMirror 6 bundle (tools/vendor-cm6.mjs).
// Exports exactly the surface MDView exposes on window.MDViewCM — no default export.
export { EditorView, keymap, placeholder } from '@codemirror/view';
export { EditorState, Text } from '@codemirror/state';
export { history, historyKeymap, defaultKeymap, indentWithTab, undo, redo } from '@codemirror/commands';
export { markdown, markdownLanguage } from '@codemirror/lang-markdown';
// NOTE: the task spec said '@lezer/highlight', but that package exports only
// Tag/tags/highlightCode/highlightTree/styleTags/etc. — syntaxHighlighting and
// defaultHighlightStyle live in '@codemirror/language' (already a pinned dep).
// Export surface on window.MDViewCM is unchanged.
export { syntaxHighlighting, defaultHighlightStyle } from '@codemirror/language';
