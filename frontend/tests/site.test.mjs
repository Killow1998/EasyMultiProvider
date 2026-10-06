import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile,access} from 'node:fs/promises';
import {JSDOM} from 'jsdom';
const site=new URL('../site/',import.meta.url);
test('static product page has real local links/assets, labeled examples, and no network runtime',async()=>{
 const html=await readFile(new URL('index.html',site),'utf8');const dom=new JSDOM(html,{url:'http://localhost/site/'}),d=dom.window.document;
 for(const e of d.querySelectorAll('[href],[src]')){const value=e.getAttribute('href')??e.getAttribute('src');if(value.startsWith('#')){if(value.length>1)assert.ok(d.getElementById(value.slice(1)),value);}else if(!value.startsWith('https:'))await access(new URL(value,site));else assert.match(value,/^https:\/\/github.com\/Killow1998\/EasyMultiProvider(?:$|\/)/);}
 assert.match(d.body.textContent,/没有包含这套界面的独立安装包/);assert.match(d.body.textContent,/演示数据/);assert.match(d.body.textContent,/尚未提交或发布/);
 const js=await readFile(new URL('site.js',site),'utf8');assert.doesNotMatch(js,/fetch\(|XMLHttpRequest|setInterval|https?:/);dom.window.close();
});
