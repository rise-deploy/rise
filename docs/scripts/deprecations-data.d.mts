import type { ChangelogDocument } from './changelog-data.mjs';
export interface DeprecationEvent {
  kind: 'introduced' | 'deprecated' | 'default-disabled' | 'removed';
  state: 'unknown' | 'unscheduled' | 'planned' | 'upcoming' | 'released';
  version: string | null;
  date: string | null;
  changeId?: string;
  firstCandidate?: { version: string; date: string };
}
export interface DeprecationRecord {
  id: string;
  title: string;
  availability: string;
  migration: string;
  events: DeprecationEvent[];
}
export const lifecycleLabels: Record<DeprecationEvent['kind'], string>;
export function deriveDeprecations(documents: ChangelogDocument[]): DeprecationRecord[];
export function deprecationStatus(record: DeprecationRecord): 'deprecated' | 'removed';
export function lifecycleVersionLabel(event: DeprecationEvent): string;
export function changelogChangeLink(changeId: string | undefined, documents: ChangelogDocument[]): string | null;
export function renderDeprecationsMarkdown(records: DeprecationRecord[], options?: { documents?: ChangelogDocument[] }): string;
