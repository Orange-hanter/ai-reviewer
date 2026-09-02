function continuityKey(entity) {
  return `${entity.qualifiedName || entity.label}\0${entity.kind}\0${entity.signature || ''}`;
}

export function reconcileIdentity(currentEntities, previousEntities = []) {
  const previousByObserved = new Map(previousEntities.map((entity) => [entity.observedId, entity]));
  const previousByContinuity = new Map();
  for (const entity of previousEntities) {
    const key = continuityKey(entity);
    if (!previousByContinuity.has(key)) previousByContinuity.set(key, []);
    previousByContinuity.get(key).push(entity);
  }
  const matchedPrevious = new Set();
  const events = [];
  const entities = currentEntities.map((entity) => {
    const exact = previousByObserved.get(entity.observedId);
    if (exact) {
      matchedPrevious.add(exact.observedId);
      return { ...entity, lineageId: exact.lineageId || exact.observedId };
    }
    const candidates = (previousByContinuity.get(continuityKey(entity)) || [])
      .filter((candidate) => !matchedPrevious.has(candidate.observedId));
    if (candidates.length === 1) {
      const previous = candidates[0];
      matchedPrevious.add(previous.observedId);
      const kind = previous.path !== entity.path
        ? (previous.label === entity.label ? 'moved' : 'renamed')
        : 'renamed';
      events.push({
        kind,
        from: previous.observedId,
        to: entity.observedId,
        confidence: 'extracted',
        evidence: [`identity:${previous.path}:${previous.label}->${entity.path}:${entity.label}`],
      });
      return { ...entity, lineageId: previous.lineageId || previous.observedId };
    }
    if (candidates.length > 1) {
      events.push({
        kind: 'identity_unresolved',
        from: null,
        to: entity.observedId,
        confidence: 'ambiguous',
        evidence: candidates.map((candidate) => `identity-candidate:${candidate.observedId}`).sort(),
      });
      return { ...entity, lineageId: null };
    }
    events.push({
      kind: 'created',
      from: null,
      to: entity.observedId,
      confidence: 'exact',
      evidence: [`identity:new:${entity.observedId}`],
    });
    return entity;
  });
  for (const previous of previousEntities) {
    if (matchedPrevious.has(previous.observedId)) continue;
    events.push({
      kind: 'removed',
      from: previous.observedId,
      to: null,
      confidence: 'extracted',
      evidence: [`identity:absent:${previous.observedId}`],
    });
  }
  events.sort((a, b) => `${a.kind}:${a.from || ''}:${a.to || ''}`.localeCompare(`${b.kind}:${b.from || ''}:${b.to || ''}`));
  return { entities, events };
}
