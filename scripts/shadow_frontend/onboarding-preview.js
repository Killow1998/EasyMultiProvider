// This origin has its own disposable backend. Never loaded in real-data Shadow.
window.EMP_PREVIEW=true;
const shadowServicesDemoActive=false;
const fixture=window.EMP_ONBOARDING_FIXTURE;
const fixtureBanner=document.createElement('p');fixtureBanner.className='muted';fixtureBanner.id='onboarding_fixture_banner';
function describeFixture(){
  fixtureBanner.innerHTML=esc(tr('隔离验收 · 合成账号；装置码不用于真实登录，8 秒后模拟成功。迁移仅连接本机的两个测试实例。','Isolated acceptance · Synthetic accounts. Device codes are not real; success is simulated after 8 seconds. Migration connects only the two local test instances.'))+` <a href="${esc(fixture.peer)}?reviewAction=lan" target="_blank" rel="noopener">${tr('打开迁移接收端','Open migration receiver')}</a> · <a href="${esc(fixture.back)}">${tr('返回真实资料 Shadow','Return to real-data Shadow')}</a>`;
}
document.querySelector('.page-header').after(fixtureBanner);describeFixture();
const fixtureFetch=window.fetch.bind(window);
window.fetch=(path,options={})=>{
  if(String(path).startsWith('/api/migration/lan/')&&options.body){
    const body=JSON.parse(options.body);
    if(String(path).endsWith('/start'))body.address='127.0.0.1';
    if(String(path).endsWith('/receive')){
      try{const encoded=body.code.slice(1).replace(/-/g,'+').replace(/_/g,'/'),bytes=atob(encoded);if(bytes.charCodeAt(1)!==127)throw new Error();}
      catch(_){return Promise.resolve(new Response(JSON.stringify({error:{message:tr('此验收页只接收另一测试实例的代码。','This acceptance page only receives codes from the other test instance.')}}),{status:400,headers:{'Content-Type':'application/json'}}));}
    }
    options={...options,body:JSON.stringify(body)};
  }
  return fixtureFetch(path,options);
};
document.addEventListener('click',event=>{
  if(event.target.closest('a[href^="https://auth.openai.com/"]')){event.preventDefault();notice(tr('这是合成登录；无需真实登录，稍候会模拟成功。','This is a synthetic sign-in. No real sign-in is needed; completion is simulated shortly.'));}
},true);
const fixtureFill=fill;let fixtureStartup=true;
fill=function(){fixtureFill();describeFixture();if(fixtureStartup&&state){fixtureStartup=false;const action=new URLSearchParams(location.search).get('reviewAction');if(action==='onboarding')nativeLogin.manage();if(action==='lan')migration.open();if(action==='gateway'){settings.open();document.querySelector('[data-gateway-open]').click();}}};
