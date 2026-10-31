// Shared rules for the roadmap pages. A roadmap is a list of stages, such as
// milestones or slices, each with items that are ticked as they ship.
import { domainModelingRoadmap } from './domain-modeling.mjs';
import { orchestrationRoadmap } from './orchestration.mjs';

export const roadmaps = [orchestrationRoadmap, domainModelingRoadmap];

export const issueUrl = (number) => `https://github.com/NiklasRosenstein/nyl/issues/${number}`;

export const statusLabels = { done: 'Done', 'in-progress': 'In progress', planned: 'Planned' };

const byId = (id) => roadmaps.find((roadmap) => roadmap.id === id);

/** Done and total items of a stage; a delegating stage counts its roadmap's. */
export function stageProgress(stage) {
  if (stage.delegate) return roadmapProgress(byId(stage.delegate));
  return { done: stage.items.filter((item) => item.done).length, total: stage.items.length };
}

/** Done and total items across a roadmap's own stages. */
export function roadmapProgress(roadmap) {
  return roadmap.stages
    .filter((stage) => !stage.delegate)
    .map(stageProgress)
    .reduce((sum, progress) => ({ done: sum.done + progress.done, total: sum.total + progress.total }), {
      done: 0,
      total: 0,
    });
}

/**
 * A stage is done only when it says so, because a milestone is done when its
 * exit criterion holds, not merely when its items are ticked. It is in progress
 * once any item is done. A delegating stage follows its roadmap: done when the
 * roadmap says so, in progress once any of its stages is.
 */
export function stageStatus(stage) {
  if (stage.delegate) {
    const roadmap = byId(stage.delegate);
    if (roadmap.done) return 'done';
    return roadmap.stages.some((child) => stageStatus(child) !== 'planned') ? 'in-progress' : 'planned';
  }
  if (stage.done) return 'done';
  return stage.items.some((item) => item.done) ? 'in-progress' : 'planned';
}

/** The stages of other roadmaps that a roadmap's stages need, in ID order. */
export function prerequisites(roadmap) {
  const own = new Set(roadmap.stages.map((stage) => stage.id));
  const needs = roadmap.stages.flatMap((stage) => stageNeeds(stage)).filter((id) => !own.has(id));
  return [...new Set(needs)].sort((a, b) => a.localeCompare(b, 'en', { numeric: true }));
}

/** A stage's title; a delegating stage takes its roadmap's. */
export function stageTitle(stage) {
  return stage.delegate ? byId(stage.delegate).title : stage.title;
}

/** A stage's needs; a delegating stage needs its roadmap's prerequisites. */
export function stageNeeds(stage) {
  return stage.delegate ? prerequisites(byId(stage.delegate)) : stage.needs;
}

/** The stage with this ID in any roadmap, with the roadmap that holds it. */
export function findStage(id) {
  for (const roadmap of roadmaps) {
    const stage = roadmap.stages.find((candidate) => candidate.id === id);
    if (stage) return { roadmap, stage };
  }
  return undefined;
}

/** The roadmap a delegating stage points to. */
export function delegateRoadmap(stage) {
  return byId(stage.delegate);
}

/** Splits roadmap text into plain runs, `code` spans, and [links](url). */
export function inlineParts(text) {
  const parts = [];
  const pattern = /`([^`]+)`|\[([^\]]+)\]\(([^)]+)\)/g;
  let last = 0;
  for (const match of text.matchAll(pattern)) {
    if (match.index > last) parts.push({ kind: 'text', value: text.slice(last, match.index) });
    if (match[1] !== undefined) parts.push({ kind: 'code', value: match[1] });
    else parts.push({ kind: 'link', value: match[2], href: match[3], children: inlineParts(match[2]) });
    last = match.index + match[0].length;
  }
  if (last < text.length) parts.push({ kind: 'text', value: text.slice(last) });
  return parts;
}
