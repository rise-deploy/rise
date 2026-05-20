import { useMemo } from 'react';
import CodeMirror from '@uiw/react-codemirror';
import { json, jsonParseLinter } from '@codemirror/lang-json';
import { linter } from '@codemirror/lint';
import { EditorView } from '@codemirror/view';
import { usePrefs } from '../lib/prefs';

const baseTheme = EditorView.theme({
    '&': { fontSize: '12.5px', backgroundColor: 'var(--surface)' },
    '.cm-scroller': { fontFamily: "'JetBrains Mono', ui-monospace, SFMono-Regular, monospace", lineHeight: '1.65' },
    '.cm-gutters': { border: 'none', backgroundColor: 'var(--surface-2)' },
    '&.cm-editor.cm-focused': { outline: 'none' },
});

// An empty document is treated as "no error" — an empty optional JSON field
// should not show a parse error until the user actually types something.
const parseLint = jsonParseLinter();
const tolerantJsonLinter = (view: any) =>
    view.state.doc.toString().trim() === '' ? [] : parseLint(view);

export function JsonEditor({
    value,
    onChange,
    readOnly = false,
    minHeight = '150px',
    maxHeight = '420px',
    ariaLabel,
}: {
    value: string;
    onChange?: (value: string) => void;
    readOnly?: boolean;
    minHeight?: string;
    maxHeight?: string;
    ariaLabel?: string;
}) {
    const [prefs] = usePrefs();

    const extensions = useMemo(() => {
        const ext: any[] = [json(), baseTheme, EditorView.lineWrapping];
        if (!readOnly) ext.push(linter(tolerantJsonLinter));
        return ext;
    }, [readOnly]);

    return (
        <div
            className="r-json-editor"
            style={{ border: '1px solid var(--border)', borderRadius: 'var(--radius-sm)', overflow: 'hidden' }}
        >
            <CodeMirror
                value={value}
                onChange={onChange}
                readOnly={readOnly}
                editable={!readOnly}
                theme={prefs.theme === 'dark' ? 'dark' : 'light'}
                extensions={extensions}
                minHeight={minHeight}
                maxHeight={maxHeight}
                basicSetup={{
                    lineNumbers: true,
                    foldGutter: false,
                    highlightActiveLine: !readOnly,
                    highlightActiveLineGutter: !readOnly,
                    autocompletion: false,
                }}
                aria-label={ariaLabel}
            />
        </div>
    );
}

export default JsonEditor;
