/// <reference types="node" />
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ATTENTION_MAX_AGE_MS, attentionFor, deploymentAttentionKey, projectAttentionKey, projectNeedsAttention } from './home-attention';
import type { Deployment, Project } from './project-context';

const NOW = Date.parse('2026-10-29T12:00:00Z');
function deployment(fields: Partial<Deployment> = {}): Deployment {
    return {
        id: 'deployment-1', deployment_id: '20261029-100000', project: 'project-1',
        status: 'Failed', environment: 'production', deployment_group: 'default',
        created: new Date(NOW - 2 * 60 * 60 * 1000).toISOString(), is_active: false,
        ...fields,
    } as Deployment;
}

test('failed attempts expire at the seven-day boundary, including preview groups', () => {
    const atLimit = deployment({ created: new Date(NOW - ATTENTION_MAX_AGE_MS).toISOString(), deployment_group: 'mr/274' });
    assert.equal(attentionFor('app', [atLimit], NOW).length, 1);
    assert.equal(attentionFor('app', [atLimit], NOW + 1).length, 0);
    assert.equal(attentionFor('app', [deployment({ created: 'invalid' })], NOW).length, 0);
});

test('active unhealthy service and in-progress work remain visible regardless of age', () => {
    const created = new Date(NOW - 30 * 86400000).toISOString();
    assert.equal(attentionFor('app', [deployment({ created, status: 'Unhealthy', is_active: true })], NOW).length, 1);
    assert.equal(attentionFor('app', [deployment({ created, status: 'Unhealthy' })], NOW).length, 0);
    for (const status of ['Pending', 'Building', 'Pushing', 'Pushed', 'Deploying']) {
        assert.equal(attentionFor('app', [deployment({ created, status })], NOW).length, 1, status);
    }
});

test('only the newest attempt in each environment and group produces a notice', () => {
    const failed = deployment();
    const recovered = deployment({ id: 'recovered', status: 'Healthy', is_active: true, created: new Date(NOW).toISOString() });
    assert.deepEqual(attentionFor('app', [failed, recovered], NOW), []);
    const preview = deployment({ id: 'preview', deployment_group: 'mr/274' });
    const staging = deployment({ id: 'staging', environment: 'staging' });
    const result = attentionFor('app', [failed, recovered, preview, staging], NOW);
    assert.deepEqual(result.map(a => a.deployment.id).sort(), ['preview', 'staging']);
});

test('a failed attempt retains the older serving deployment in the same group', () => {
    const failed = deployment();
    const live = deployment({ id: 'live', status: 'Healthy', is_active: true, created: new Date(NOW - 86400000).toISOString() });
    const [notice] = attentionFor('app', [live, failed], NOW);
    assert.equal(notice.deployment, failed);
    assert.equal(notice.serving, live);
    assert.deepEqual(attentionFor('app', [deployment({ is_active: true })], NOW), []);
});

test('project fallbacks cannot resurrect an expired or dismissed deployment failure', () => {
    const p = { id: 'project-1', name: 'app', status: 'Running', latest_deployment_id: 'old', latest_deployment_status: 'Failed' } as Project;
    assert.equal(projectNeedsAttention(p), false);
    assert.equal(projectNeedsAttention({ ...p, status: 'Failed' }), false);
    assert.equal(projectNeedsAttention({ ...p, status: 'Failed', active_deployment_status: 'Unhealthy' }), true);
    assert.equal(projectNeedsAttention({ ...p, status: 'Failed', latest_deployment_id: undefined }), true);
    assert.equal(projectNeedsAttention({ ...p, status: 'Deleting' }), true);
});

test('dismissals apply to a single project, deployment and status', () => {
    const d = deployment();
    const key = deploymentAttentionKey('app', d);
    assert.notEqual(deploymentAttentionKey('other', d), key);
    assert.notEqual(deploymentAttentionKey('app', { ...d, id: 'new' }), key);
    assert.notEqual(deploymentAttentionKey('app', { ...d, status: 'Unhealthy' }), key);
    assert.equal(deploymentAttentionKey('app', { ...d, updated: new Date(NOW).toISOString() }), key);
    const p = { id: 'project-1', status: 'Deleting', updated: 'today' } as Project;
    assert.notEqual(projectAttentionKey(p), projectAttentionKey({ ...p, updated: 'tomorrow' }));
});
