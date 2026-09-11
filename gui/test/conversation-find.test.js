import test from 'node:test';
import assert from 'node:assert/strict';
import { literalMatchParts, conversationMatches, stepConversationMatch } from '../src/renderer/lib/conversation-find.js';

test('literal conversation search preserves Unicode graphemes and markup', () => {
  const original = '😀 Café cafe\u0301 CAFÉ <img src=x> İstanbul';
  const parts = literalMatchParts(original, 'CAFE');
  assert.equal(parts.map(part => part.text).join(''), original);
  assert.deepEqual(parts.filter(part => part.matched).map(part => part.text), ['Café','cafe\u0301','CAFÉ']);
  assert.equal(literalMatchParts(original, 'istanbul').filter(part => part.matched)[0].text, 'İstanbul');
  assert.deepEqual(literalMatchParts('<img src=x>', '<img').filter(part => part.matched), [{text:'<img',matched:true}]);
});
test('conversation matching is literal, includes repeated phrases, and has no regex semantics', () => {
  assert.equal(literalMatchParts('a.* a.*', 'a.*').filter(part => part.matched).length, 2);
  assert.equal(literalMatchParts('anything', '.*').some(part => part.matched), false);
  assert.equal(literalMatchParts('words', ' ').some(part => part.matched), false);
  assert.equal(literalMatchParts('👩‍💻 hello', '👩').map(part => part.text).join(''), '👩‍💻 hello');
});
test('complete conversation results sort chronologically without mutating rows', () => {
  const rows = [{id:3,t_ms:3,text:'old phrase'}, {id:1,t_ms:1,text:'phrase twice phrase'}, {id:2,t_ms:2,text:'other'}];
  assert.deepEqual(conversationMatches(rows,'phrase').map(item => item.row.id),[1,3]);
  assert.deepEqual(rows.map(row => row.id),[3,1,2]);
  assert.equal(conversationMatches(rows,'missing').length,0);
});
test('match stepping handles forward/backward wrap and empty conversations', () => {
  assert.equal(stepConversationMatch(0,-1,3),2);
  assert.equal(stepConversationMatch(2,1,3),0);
  assert.equal(stepConversationMatch(0,1,1),0);
  assert.equal(stepConversationMatch(-1,1,0),-1);
});
