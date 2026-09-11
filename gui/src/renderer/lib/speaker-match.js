// Identical matching for speaker filters and identity selection sheets.
export function foldSpeakerText(value) {
  return String(value ?? '').normalize('NFKD').replace(/\p{M}/gu, '').toLocaleLowerCase().replace(/[_\s]+/gu, ' ').trim();
}
export function speakerMatches(sp, query, label = '') {
  const terms=foldSpeakerText(query).split(/\s+/u).filter(Boolean);
  const haystack=foldSpeakerText(`${sp?.name ?? ''} ${sp?.auto ?? ''} ${sp?.id ?? ''} ${sp?.id == null ? '' : `voice ${sp.id} speaker ${sp.id}`} ${label}`);
  return terms.every(term=>haystack.includes(term));
}
