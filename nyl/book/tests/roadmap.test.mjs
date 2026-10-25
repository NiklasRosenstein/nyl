import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { domainResourcesRoadmap, roadmapProgress, sliceStatus, statusLabels } from '../src/lib/roadmaps/domain-resources.mjs';

const roadmapMarkdown = readFileSync(new URL('../../../ROADMAP.md', import.meta.url), 'utf8');

// The milestone table of ROADMAP.md: ID to status, as `done`, `in-progress`, or `planned`.
function milestoneStatuses() {
  const statuses = new Map();
  for (const line of roadmapMarkdown.split('\n')) {
    const cells = line.split('|').map((cell) => cell.trim());
    if (/^M\d+$/.test(cells[1] ?? '')) statuses.set(cells[1], cells[3].toLowerCase().replace(' ', '-'));
  }
  return statuses;
}

test('the domain resources roadmap is well formed', () => {
  const ids = domainResourcesRoadmap.slices.map((slice) => slice.id);
  assert.deepEqual(ids, ids.map((_, index) => `R${index + 1}`), 'slices are numbered R1, R2, … in order');
  const prerequisites = new Set(domainResourcesRoadmap.prerequisites.map((prerequisite) => prerequisite.id));
  for (const [index, slice] of domainResourcesRoadmap.slices.entries()) {
    assert.ok(slice.deliverables.length, `${slice.id} has deliverables`);
    for (const need of slice.needs) {
      if (need.startsWith('R')) {
        assert.ok(ids.indexOf(need) >= 0 && ids.indexOf(need) < index, `${slice.id} needs an earlier slice, not ${need}`);
      } else {
        assert.ok(prerequisites.has(need), `${slice.id} needs ${need}, which is not a listed prerequisite`);
      }
    }
    for (const { text } of slice.deliverables) {
      assert.equal(text.split('`').length % 2, 1, `${slice.id}: unbalanced code span in "${text}"`);
    }
  }
  for (const prerequisite of domainResourcesRoadmap.prerequisites) {
    assert.ok(prerequisite.status in statusLabels, `${prerequisite.id} status ${prerequisite.status}`);
  }
});

test('prerequisite statuses match the ROADMAP.md milestone table', () => {
  const statuses = milestoneStatuses();
  for (const prerequisite of domainResourcesRoadmap.prerequisites) {
    assert.equal(prerequisite.status, statuses.get(prerequisite.id), `${prerequisite.id} in ROADMAP.md`);
  }
  assert.ok(statuses.has(domainResourcesRoadmap.milestone), `${domainResourcesRoadmap.milestone} is listed in ROADMAP.md`);
});

test('slice status and overall progress follow the deliverables', () => {
  const slice = (...done) => ({ deliverables: done.map((value) => ({ text: 'x', done: value })) });
  assert.equal(sliceStatus(slice(false, false)), 'planned');
  assert.equal(sliceStatus(slice(true, false)), 'in-progress');
  assert.equal(sliceStatus(slice(true, true)), 'done');
  assert.deepEqual(roadmapProgress({ slices: [slice(true, false), slice(true)] }), { done: 2, total: 3 });
});
