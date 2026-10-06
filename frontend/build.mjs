import {readFile,mkdir,cp,writeFile} from 'node:fs/promises';
import {Script} from 'node:vm';
const web=new URL('../crates/emp-app/web/',import.meta.url),dist=new URL('./dist/',import.meta.url),html=await readFile(new URL('index.html',web),'utf8');
for(const m of html.matchAll(/<script(?:\s+src="([^"]+)")?>([\s\S]*?)<\/script>/g))new Script(m[1]?await readFile(new URL(m[1].replace('/assets/',''),web),'utf8'):m[2],{filename:m[1]||'inline-page.js'});
await mkdir(new URL('management/assets/',dist),{recursive:true});await writeFile(new URL('management/index.html',dist),html);
for(const n of ['style.css','management-client.js','settings.js','request-details.js','diagnostics.js'])await cp(new URL(n,web),new URL('management/assets/'+n,dist));
await cp(new URL('./site/',import.meta.url),new URL('site/',dist),{recursive:true});
console.log('Built dist/management and standalone dist/site. Rust embeds source web assets directly.');
