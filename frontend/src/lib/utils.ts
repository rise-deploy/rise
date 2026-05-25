/** Drop a leading http:// or https:// for display; other schemes are kept. */
export function stripUrlScheme(url: string | null | undefined): string {
  return (url || '').replace(/^https?:\/\//i, '');
}

/** Returns true if the URL uses http: or https: scheme (safe to render as a clickable link). */
export function isSafeUrl(url: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.protocol === 'http:' || parsed.protocol === 'https:';
  } catch {
    return false;
  }
}

export async function copyToClipboard(text: string): Promise<void> {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text);
  }

  const textarea = document.createElement('textarea');
  textarea.value = text;
  textarea.style.position = 'fixed';
  textarea.style.opacity = '0';
  document.body.appendChild(textarea);
  textarea.select();

  try {
    document.execCommand('copy');
    document.body.removeChild(textarea);
  } catch {
    document.body.removeChild(textarea);
    throw new Error('Clipboard not available');
  }
}

export function formatDate(dateString: string | null | undefined): string {
  if (!dateString) return '-';
  const date = new Date(dateString);
  if (Number.isNaN(date.getTime())) return '-';
  return date.toLocaleString();
}

export function formatISO8601(dateString: string): string {
  const date = new Date(dateString);
  if (Number.isNaN(date.getTime())) return '-';
  return date.toISOString();
}

export function formatRelativeTimeRounded(dateString: string): string {
  const date = new Date(dateString);
  if (Number.isNaN(date.getTime())) return '-';

  const now = new Date();
  const diffMs = date.getTime() - now.getTime();
  const absMs = Math.abs(diffMs);
  const absSec = absMs / 1000;
  const absMin = absSec / 60;
  const absHour = absMin / 60;
  const absDay = absHour / 24;
  const absMonth = absDay / 30;
  const absYear = absDay / 365;

  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: 'auto', style: 'long' });

  if (absYear >= 1) return rtf.format(Math.round(diffMs / (1000 * 60 * 60 * 24 * 365)), 'year');
  if (absMonth >= 1) return rtf.format(Math.round(diffMs / (1000 * 60 * 60 * 24 * 30)), 'month');
  if (absDay >= 1) return rtf.format(Math.round(diffMs / (1000 * 60 * 60 * 24)), 'day');
  if (absHour >= 1) return rtf.format(Math.round(diffMs / (1000 * 60 * 60)), 'hour');
  if (absMin >= 1) return rtf.format(Math.round(diffMs / (1000 * 60)), 'minute');
  return rtf.format(Math.round(diffMs / 1000), 'second');
}

export function formatTimeRemaining(expiresAt: string | null | undefined): string | null {
  if (!expiresAt) return null;

  const now = new Date();
  const expiryDate = new Date(expiresAt);
  const diffMs = expiryDate.getTime() - now.getTime();
  const diffSec = Math.floor(Math.abs(diffMs) / 1000);
  const diffMin = Math.floor(diffSec / 60);
  const diffHour = Math.floor(diffMin / 60);
  const diffDay = Math.floor(diffHour / 24);

  const isExpired = diffMs < 0;
  const prefix = isExpired ? 'expired ' : 'in ';
  const suffix = isExpired ? ' ago' : '';

  if (diffDay > 0) return `${prefix}${diffDay} day${diffDay > 1 ? 's' : ''}${suffix}`;
  if (diffHour > 0) return `${prefix}${diffHour} hour${diffHour > 1 ? 's' : ''}${suffix}`;
  if (diffMin > 0) return `${prefix}${diffMin} minute${diffMin > 1 ? 's' : ''}${suffix}`;
  return `${prefix}${diffSec} second${diffSec !== 1 ? 's' : ''}${suffix}`;
}
