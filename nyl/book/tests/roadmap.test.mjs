import test from 'node:test';
import assert from 'node:assert/strict';
import { findStage, inlineParts, roadmapProgress, roadmaps, stageStatus } from '../src/lib/roadmaps/index.mjs';

const allStages = roadmaps.flatMap((roadmap) => roadmap.stages.map((stage) => ({ roadmap, stage })));
const texts = (stage) => [stage.summary, stage.exitCriterion, ...(stage.items ?? []).map((item) => item.text)].filter(Boolean);

test('roadmap stage IDs are unique across roadmaps', () => {
  const ids = allStages.map(({ stage }) => stage.id);
  assert.deepEqual(ids, [...new Set(ids)]);
});

test('every dependency names a known stage, and an earlier one within its roadmap', () => {
  for (const { roadmap, stage } of allStages) {
    const order = roadmap.stages.map((candidate) => candidate.id);
    for (const need of [...stage.needs, ...(roadmap.prerequisites ?? [])]) {
      const found = findStage(need);
      assert.ok(found, `${stage.id} needs unknown stage ${need}`);
      if (found.roadmap === roadmap && stage.needs.includes(need)) {
        assert.ok(order.indexOf(need) < order.indexOf(stage.id), `${stage.id} needs a later stage, ${need}`);
      }
    }
  }
});

test('a stage marked done has every item done', () => {
  for (const { stage } of allStages.filter(({ stage }) => stage.done)) {
    assert.ok(stage.items.length, `${stage.id} has items`);
    assert.ok(stage.items.every((item) => item.done), `${stage.id} is done with unticked items`);
  }
});

test('a delegating stage points at a roadmap and has no items of its own', () => {
  for (const { stage } of allStages.filter(({ stage }) => stage.delegate)) {
    assert.ok(roadmaps.some((roadmap) => roadmap.id === stage.delegate), `${stage.id} delegates to ${stage.delegate}`);
    assert.equal(stage.items, undefined, `${stage.id} tracks its items in ${stage.delegate}`);
  }
});

test('roadmap text uses only code spans and links that resolve on the site', () => {
  for (const { stage } of allStages) {
    for (const text of texts(stage)) {
      assert.equal(text.replace(/\[[^\]]+\]\([^)]+\)/g, '').split('`').length % 2, 1, `${stage.id}: unbalanced code span in "${text}"`);
      for (const part of inlineParts(text).filter((part) => part.kind === 'link')) {
        assert.match(part.href, /^(https:\/\/|\/nyl\/)/, `${stage.id}: relative link ${part.href} does not resolve on the site`);
      }
    }
  }
});

test('status and progress follow the items, and done stays explicit', () => {
  const stage = (done, ...items) => ({ done, items: items.map((value) => ({ text: 'x', done: value })) });
  assert.equal(stageStatus(stage(false, false, false)), 'planned');
  assert.equal(stageStatus(stage(false, true, false)), 'in-progress');
  assert.equal(stageStatus(stage(false, true, true)), 'in-progress');
  assert.equal(stageStatus(stage(true, true, true)), 'done');
  assert.deepEqual(roadmapProgress({ stages: [stage(false, true, false), stage(true, true)] }), { done: 2, total: 3 });
});

test('inline text splits into text, code, and links', () => {
  assert.deepEqual(inlineParts('Run `nyl plan` per [guide](/nyl/x/).'), [
    { kind: 'text', value: 'Run ' },
    { kind: 'code', value: 'nyl plan' },
    { kind: 'text', value: ' per ' },
    { kind: 'link', value: 'guide', href: '/nyl/x/', children: [{ kind: 'text', value: 'guide' }] },
    { kind: 'text', value: '.' },
  ]);
});
