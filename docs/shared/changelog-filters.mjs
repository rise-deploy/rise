/**
 * Universal changes apply when filtering for any deployment backend.
 * @param {readonly string[]} supported
 * @param {string} selected
 */
export function matchesBackend(supported, selected) {
  return selected === 'all' || supported.includes('all') || supported.includes(selected);
}
