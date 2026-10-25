// Shared rules for the roadmap pages. A roadmap is a list of stages, such as
// milestones or slices, each with items that are ticked as they ship.
import { domainResourcesRoadmap } from './domain-resources.mjs';
import { orchestrationRoadmap } from './orchestration.mjs';

export const roadmaps = [orchestrationRoadmap, domainResourcesRoadmap];

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
 * once any item is done.
 */
export function stageStatus(stage) {
  if (stage.delegate) {
    const stages = byId(stage.delegate).stages;
    if (stages.every((child) => stageStatus(child) === 'done')) return 'done';
    return stages.some((child) => stageStatus(child) !== 'planned') ? 'in-progress' : 'planned';
  }
  if (stage.done) return 'done';
  return stage.items.some((item) => item.done) ? 'in-progress' : 'planned';
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
