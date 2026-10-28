#!/usr/bin/env node
import { readFile } from 'node:fs/promises';
import { loadChangelog, publishChangelogRelease, renderChangelogMarkdown } from './changelog-data.mjs';

try {
  const args = process.argv.slice(2);
  if (args.length === 1 && args[0] === '--unreleased') {
    const upcoming = (await loadChangelog()).find(document => document.id === 'unreleased');
    if (!upcoming) throw new Error('Unreleased changelog data is required');
    process.stdout.write(renderChangelogMarkdown(upcoming));
  } else {
    if (args.length < 2 || args.length > 3) throw new Error('Usage: changelog-release.mjs <version> <release-notes-file> [YYYY-MM-DD]');
    const [version, notesFile, date = new Date().toISOString().slice(0, 10)] = args;
    const notes = await readFile(notesFile, 'utf8');
    const release = await publishChangelogRelease(version, notes, date);
    console.log(`Published ${release.version} changelog data and release page.`);
  }
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
