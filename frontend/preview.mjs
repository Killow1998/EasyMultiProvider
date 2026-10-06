import http from 'node:http';
import {readFile} from 'node:fs/promises';
import {createMock} from './tests/fixtures.mjs';
const port=Number(process.env.EMP_FRONTEND_PORT||4318),web=new URL('../crates/emp-app/web/',import.meta.url),site=new URL('./site/',import.meta.url),scenarios=['matched','pending','native','empty','stale','failed','conflict','many','offline','stopping'];let mock=createMock();
http.createServer(async(req,res)=>{
 const url=new URL(req.url,'http://127.0.0.1'),path=url.pathname;
 try{
  if(path.startsWith('/api/')){
   if(path==='/api/accounts/events'){if(mock.scenario==='offline'){res.destroy();return;}res.writeHead(200,{'Content-Type':'text/event-stream','Cache-Control':'no-store'});res.write(': synthetic preview\n\n');const timer=setInterval(()=>res.write(': keepalive\n\n'),15000);req.on('close',()=>clearInterval(timer));return;}
   const buffers=[];let bytes=0;for await(const chunk of req){bytes+=chunk.length;if(bytes>2_000_000){res.writeHead(413);res.end();return;}buffers.push(chunk);}
   const data=await mock.handle(path+url.search,{method:req.method,body:Buffer.concat(buffers).toString()||undefined});res.writeHead(data.__status||200,{'Content-Type':'application/json'});res.end(JSON.stringify(data));return;
  }
  let file,content;
  if(path==='/'){
   const scenario=scenarios.includes(url.searchParams.get('scenario'))?url.searchParams.get('scenario'):'matched';mock=createMock(scenario==='stopping'?'matched':scenario);file=new URL('index.html',web);content=await readFile(file,'utf8');
   content=content.replace('<body>',`<body><aside style="position:relative;z-index:300;background:#232429;color:#fff;padding:7px 12px;font:12px system-ui;display:flex;gap:12px;align-items:center;flex-wrap:wrap">隔离模拟预览 · 请勿导入真实凭据 ${scenario==='stopping'?'· 退出操作自动确认（仅模拟）':''} <label>场景 <select onchange="location.href='/?scenario='+this.value" style="width:auto;margin:0;padding:2px;font:12px system-ui">${scenarios.map(s=>`<option ${s===scenario?'selected':''}>${s}</option>`).join('')}</select></label><a style="color:#fff" href="/site/">产品介绍页</a></aside>`);
   if(scenario==='stopping') content=content.replace('<script>', '<script>window.confirm=()=>true; // Synthetic shutdown scenario only; production confirmations are unchanged.\n');
   content=content.replace('establishSession().then(load)',"localStorage.setItem('emp_management_session_v1','synthetic-preview-session');\nestablishSession().then(load)");
  }else if(/^\/assets\/[a-z-]+\.(js|css)$/.test(path))file=new URL(path.slice(8),web);
  else if(path.startsWith('/site/')&&!decodeURIComponent(path).includes('..'))file=new URL(path.slice(6)||'index.html',site);
  else{res.writeHead(404);res.end('Not found');return;}
  content??=await readFile(file);res.writeHead(200,{'Content-Type':({html:'text/html; charset=utf-8',css:'text/css',js:'text/javascript',svg:'image/svg+xml',md:'text/plain; charset=utf-8'})[file.pathname.split('.').at(-1)]||'application/octet-stream','Cache-Control':'no-store'});res.end(content);
 }catch(error){if(path.startsWith('/api/')){res.destroy();return;}res.writeHead(404);res.end('Not found');}
}).listen(port,'127.0.0.1',()=>console.log(`Isolated UI: http://127.0.0.1:${port}/\nProduct page: http://127.0.0.1:${port}/site/\nNo EMP process, Codex config, or provider used.`));
