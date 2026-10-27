import { Button } from './r-ui';
import { Icon } from './icon';
import { useToast } from './toast';

export function CommandHint({ command = 'rise deploy', label = 'Deploy from your terminal' }: { command?: string; label?: string }) {
    const { showToast } = useToast();
    async function copy() {
        try {
            await navigator.clipboard.writeText(command);
            showToast('Command copied', 'success');
        } catch {
            showToast('Unable to copy. Select the command to copy it manually.', 'error');
        }
    }
    return (
        <div className="r-command-hint">
            <Icon name="terminal" size={14} />
            <span>{label}</span>
            <code className="mono">{command}</code>
            <Button variant="ghost" size="sm" icon="copy" onClick={copy} aria-label={`Copy ${command}`}>Copy</Button>
        </div>
    );
}
