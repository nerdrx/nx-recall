import test from 'node:test';
import assert from 'node:assert/strict';
import { vocabularyAssistanceState } from '../src/renderer/views/memory.js';
test('name assistance distinguishes configured hints from runtime readiness',()=>{
 assert.equal(vocabularyAssistanceState({applied_to_decoder:false}).supported,false);
 const fields={name_assistance_runtime_available:true,name_assistance_terms:['Lanalu']};
 assert.equal(vocabularyAssistanceState({...fields,name_assistance_enabled:false}).summary,'Experimental name assistance is off.');
 const active=vocabularyAssistanceState({...fields,applied_to_decoder:false,name_assistance_enabled:true});
 assert.equal(active.enabled,true);assert.equal(active.summary,'Configured for 1 selected name. Used on eligible future speech.');
 assert.equal(vocabularyAssistanceState({name_assistance_enabled:'true'}).enabled,false);
 const missing=vocabularyAssistanceState({...fields,name_assistance_enabled:true,name_assistance_runtime_available:false});
 assert.equal(missing.enabled,true);assert.equal(missing.runtimeAvailable,false);assert.match(missing.summary,/Install Local Voice first/);
 assert.match(vocabularyAssistanceState({...fields,name_assistance_enabled:true,name_assistance_terms:[]}).summary,/Add an eligible name/);
});
