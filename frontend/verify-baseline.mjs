import {execFileSync} from 'node:child_process';
const baseline='a568d005b4fc32793ec8e06db7d5891324a1c142',root=new URL('../',import.meta.url);
const git=(...args)=>execFileSync('git',args,{cwd:root,encoding:'utf8'}).trim();
const files=git('ls-tree','-r','--name-only',baseline).split('\n').filter(p=>!['crates/emp-app/web/index.html','crates/emp-app/web/style.css'].includes(p));
for(const f of files)if(git('rev-parse',`${baseline}:${f}`)!==git('hash-object',f))throw new Error(`Baseline changed: ${f}`);
console.log(`Verified ${files.length} original files byte-for-byte against ${baseline}. Rust, protocol, configuration format and core dependencies unchanged.`);
