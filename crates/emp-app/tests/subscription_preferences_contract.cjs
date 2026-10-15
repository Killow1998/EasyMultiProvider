// Preference ownership can be verified without the page, server or another editor.
const fs=require('node:fs'),path=require('node:path'),vm=require('node:vm');
const assert=require('node:assert/strict'),{test}=require('node:test');
const source=fs.readFileSync(path.join(__dirname,'../web/subscription-settings.js'),'utf8');
const apply=vm.runInNewContext(source+';applySubscriptionPreferences');
const conflicts=vm.runInNewContext(source+';subscriptionPreferenceConflicts');
const preferenceModels=vm.runInNewContext(source+';subscriptionPreferenceModels');
const plain=value=>JSON.parse(JSON.stringify(value));
function config(){return {
  native_account:{id:'@native',native:true},native_hidden_models:['keep'],native_model_context_windows:{keep:123},
  accounts:[{id:'one',hidden_models:['keep'],model_context_windows:{keep:456}},{id:'two',hidden_models:[]}],
  providers:[{id:'api',base_url:'https://fixture.invalid/v1',api_key_file:'fixture-credential-reference'}],models:[{id:'api/model',enabled:true}],
};}
test('single subscription saves preserve peers exactly; marked models synchronize in either direction',()=>{
  const initial=config(),peers=plain(initial.accounts),providers=plain(initial.providers),models=plain(initial.models);
  const single=apply(initial,{id:'@native',native:true,hidden:new Set(['luna']),windows:{luna:467368},syncModels:new Set()});
  assert.deepEqual(plain(single.accounts),peers);
  const copied=apply(config(),{id:'@native',native:true,hidden:new Set(['luna']),windows:{luna:467368},syncModels:new Set(['luna'])});
  assert.equal(copied.accounts[0].model_context_windows.keep,456);
  assert.equal(copied.accounts[1].model_context_windows.luna,467368);
  assert.deepEqual(plain(copied.accounts[0].hidden_models),['keep','luna']);
  assert.deepEqual(plain(copied.providers),providers);assert.deepEqual(plain(copied.models),models);
  const imported=apply(config(),{id:'one',native:false,hidden:new Set(),windows:{luna:400000},syncModels:new Set(['luna'])});
  assert.equal(imported.native_model_context_windows.luna,400000);assert.equal(imported.native_model_context_windows.keep,123);
  assert.equal(imported.accounts[1].model_context_windows.luna,400000);
  const blank=apply(imported,{id:'two',native:false,hidden:new Set(),windows:{},syncModels:new Set(['luna'])});
  assert.equal('luna' in blank.native_model_context_windows,false);
  assert.equal('luna' in blank.accounts[0].model_context_windows,false);
});
test('verified peers share only chosen model preferences; independent accounts and route references survive',()=>{
  const initial=config();
  initial.native_account.hidden_models=['native-hidden'];initial.native_account.model_context_windows={luna:400000};
  initial.accounts[0]={...initial.accounts[0],duplicate:true,duplicate_of:'当前 Codex 登录',auth_file:'one.enc',prefix:'one'};
  assert.equal(conflicts(initial,initial.native_account),true);
  const missing=preferenceModels([{id:'keep'}],[{hidden:new Set(['retired']),windows:{}},{hidden:new Set(),windows:{retired:123}}]);
  assert.deepEqual(plain(missing),[{id:'keep'},{id:'retired',display_name:'retired',unavailable:true}]);
  const independent=plain(initial.accounts[1]);
  const chosen=apply(initial,{id:'@native',native:true,hidden:new Set(['luna']),windows:{luna:467368},syncModels:new Set()});
  assert.deepEqual(plain(chosen.accounts[0].hidden_models),['luna']);
  assert.deepEqual(plain(chosen.accounts[0].model_context_windows),{luna:467368});
  assert.equal(chosen.accounts[0].auth_file,'one.enc');assert.equal(chosen.accounts[0].prefix,'one');
  assert.deepEqual(plain(chosen.accounts[1]),independent);
  chosen.native_account.hidden_models=['luna'];chosen.native_account.model_context_windows={luna:467368};
  assert.equal(conflicts(chosen,chosen.native_account),false);
});
