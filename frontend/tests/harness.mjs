import {JSDOM,VirtualConsole} from 'jsdom';
import {readFile} from 'node:fs/promises';
import {createMock} from './fixtures.mjs';
const web=new URL('../../crates/emp-app/web/',import.meta.url);
export async function boot(scenario='matched') {
 const mock=createMock(scenario),errors=[];let html=await readFile(new URL('index.html',web),'utf8');
 for(const m of [...html.matchAll(/<script src="\/assets\/([^"]+)"><\/script>/g)])html=html.replace(m[0],`<script>${await readFile(new URL(m[1],web),'utf8')}</script>`);
 const vc=new VirtualConsole();vc.on('jsdomError',e=>{if(e.type!=='not-implemented')errors.push(e);});
 const dom=new JSDOM(html,{url:'http://localhost/?bootstrap=fixture',runScripts:'dangerously',pretendToBeVisual:true,virtualConsole:vc,beforeParse(w){w.Headers=Headers;w.Response=Response;w.TextEncoder=TextEncoder;w.TextDecoder=TextDecoder;w.matchMedia=()=>({matches:false});w.confirm=()=>true;w.fetch=async(path,o={})=>{if(path==='/api/accounts/events')return new Promise((_resolve,reject)=>o.signal?.addEventListener('abort',()=>reject(new Error('aborted'))));const r=await mock.handle(path,o);return new Response(JSON.stringify(r),{status:r?.__status||200});};}});
 await new Promise(r=>setTimeout(r,80));return {dom,w:dom.window,d:dom.window.document,mock,errors,close(){dom.window.close();}};
}
export const settle=()=>new Promise(r=>setTimeout(r,50));
