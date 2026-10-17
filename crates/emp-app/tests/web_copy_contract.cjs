const {assert,fs,path,vm,web,translate} = require('./web_fixture.cjs');
const {test} = require('node:test');

test('all shipped copy has both locales and matching substitution names', () => {
  const entries = new Map();
  const context = vm.createContext({registerUiCopy(values) {
    for (const [key,pair] of Object.entries(values)) {
      assert.equal(entries.has(key),false,`duplicate copy key: ${key}`);
      entries.set(key,pair);
    }
  }});
  const parameters = text => [...new Set([...text.matchAll(/\{([A-Za-z][A-Za-z0-9_]*)\}/g)].map(match=>match[1]))].sort();
  for (const file of fs.readdirSync(web).filter(name=>/^copy-.*\.js$/.test(name))) {
    vm.runInContext(fs.readFileSync(path.join(web,file),'utf8'),context,{filename:file});
  }
  for (const [key,pair] of entries) {
    assert.equal(pair.length,2,key);
    assert.ok(pair.every(value=>typeof value==='string' && value.length),key);
    assert.deepEqual(parameters(pair[0]),parameters(pair[1]),key);
  }
  for (const file of fs.readdirSync(web).filter(name=>/\.(js|html)$/.test(name)&&!/^copy-/.test(name))) {
    const source=fs.readFileSync(path.join(web,file),'utf8');
    for (const match of source.matchAll(/\b(?:tr|t)\(\s*(['"])([a-zA-Z0-9_.]+)\1\s*[,)]/g)) {
      if(file==='recovery.html') continue;
      assert.ok(entries.has(match[2]),`${file}: missing copy ${match[2]}`);
    }
    assert.doesNotMatch(source,/\btr\(\s*['"`][^\n]*[\u3400-\u9fff]/,`${file}: inline translated copy`);
  }
});

test('copy substitutions retain literal user values and locale-specific wording', () => {
  assert.equal(translate('en')('core.active_request',{count:1}),'1 active request');
  assert.equal(translate('zh-CN')('core.active_requests',{count:2}),'2 个请求正在进行');
  // An arbitrary value must not become a second round of template substitution.
  const literal='$& {count} <img>';
  assert.equal(translate('en')('models.catalog_context_help',{tokens:literal,percent:95,usable:1}).includes(literal),true);
  assert.equal(translate('en')('unknown.key'),'unknown.key');
});
