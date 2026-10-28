export type ChangelogImpact = 'breaking' | 'action-required' | 'configuration' | 'none';
export type ChangelogBackend = 'all' | 'kubernetes' | 'docker' | 'ecs';
export interface ChangelogEntry {
  id: string;
  title: string;
  type: 'feature' | 'fix' | 'change' | 'maintenance' | 'limitation' | 'deprecation';
  impact: ChangelogImpact;
  backends: ChangelogBackend[];
  details: string;
  action?: string;
  migration?: string;
  feature?: string;
  stage?: 'introduced' | 'deprecated' | 'default-disabled' | 'removed';
  featureTitle?: string;
  availability?: string;
  firstCandidate?: { version: string; date: string };
  targetVersion?: string;
  archived?: boolean;
}
export interface ChangelogSection {
  id: string;
  title: string;
  introduction?: string;
  entries: ChangelogEntry[];
}
export interface ChangelogDocument {
  id: string;
  status: 'upcoming' | 'released' | 'planned';
  version: string | null;
  date: string | null;
  title: string;
  introduction: string;
  sections: ChangelogSection[];
}
export const CHANGELOG_DATA_DIR: string;
export const CHANGELOG_PAGES_DIR: string;
export const IMPACTS: ChangelogImpact[];
export const CHANGE_TYPES: ChangelogEntry['type'][];
export const BACKENDS: ChangelogBackend[];
export const DEPRECATION_STAGES: NonNullable<ChangelogEntry['stage']>[];
export const impactLabels: Record<ChangelogImpact, string>;
export function validateChangelogDocument(document: unknown): ChangelogDocument;
export function parseChangelogYaml(source: string, filename?: string): ChangelogDocument;
export function stringifyChangelogDocument(document: ChangelogDocument): string;
export function validateChangelog(documents: unknown[]): ChangelogDocument[];
export function loadChangelog(options?: { dataDir?: string }): Promise<ChangelogDocument[]>;
export function renderChangelogMarkdown(document: ChangelogDocument, options?: { includeTitle?: boolean }): string;
export function changelogMarkdownForSlug(slug: string, options?: { dataDir?: string }): Promise<string | null>;
export function releasePage(version: string, date: string): string;
export function publishChangelogRelease(version: string, notes: string, date: string, options?: { dataDir?: string; pagesDir?: string }): Promise<ChangelogDocument>;
