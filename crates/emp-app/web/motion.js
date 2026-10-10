// Static glyph mask + one moving green layer, rather than a fill animation per dot.
function enhanceLogo(svg) {
  if (!svg?.childElementCount || svg.parentElement.classList.contains('presentation-logo-shell')) return;
  const mask=svg.cloneNode(true); mask.removeAttribute('id'); mask.removeAttribute('class');
  mask.setAttribute('xmlns','http://www.w3.org/2000/svg');
  for (const dot of [...mask.children]) {
    if (!dot.hasAttribute('data-lit')) dot.remove();
    else {dot.removeAttribute('class');dot.removeAttribute('style');dot.setAttribute('fill','white');}
  }
  const shell=document.createElement('span'); shell.className='presentation-logo-shell';
  const wave=document.createElement('span'); wave.className='presentation-logo-wave'; wave.setAttribute('aria-hidden','true');
  wave.style.setProperty('--logo-mask',`url("data:image/svg+xml,${encodeURIComponent(mask.outerHTML)}")`);
  const color=document.createElement('span');color.dataset.presentationMotion='logo';wave.append(color);
  svg.replaceWith(shell); shell.append(svg,wave);
}
// One 15fps owner for the persistent textures; no independent CSS frame loops.
function installMotion(refreshCountdowns) {
  const tracked=new Set(), motion=matchMedia('(prefers-reduced-motion:reduce)');
  let effects=[], raf=null, frameTimer=null, last=-Infinity, timer=null;
  const visible=node=>!document.hidden&&!node.closest('.presentation-motion-paused');
  function active(node) {
    if(!visible(node))return false;
    if(node.dataset.presentationMotion==='logo')return node.closest('.presentation-logo-shell').querySelector('.emp-live');
    if(node.dataset.presentationMotion==='halo')return node.parentElement.dataset.activityState==='active'&&!node.parentElement.hasAttribute('data-error-message');
    return true;
  }
  function paint(time) {
    raf=null; frameTimer=null;
    if(document.hidden)return;
    let running=false;
    if(time-last>=1000/15 || motion.matches) {
      last=time;
      for(const node of effects) {
        if(!node.isConnected||!active(node))continue;
        running=true;
        if(node.dataset.presentationMotion==='ring')node.style.transform=`rotate(${motion.matches?90:time/5000*360%360}deg)`;
        else if(node.dataset.presentationMotion==='logo')node.style.transform=`translateX(${motion.matches?0:Math.cos(time/2200*Math.PI)*15}%)`;
        else {
          const wave=motion.matches?1:(1-Math.cos(time/1900*Math.PI*2))/2;
          node.style.opacity=String(.55+.45*wave);node.style.transform=`scale(${.98+.1*wave})`;
        }
      }
    } else running=effects.some(node=>node.isConnected&&active(node));
    if(running&&!motion.matches)frameTimer=setTimeout(()=>{frameTimer=null;raf=requestAnimationFrame(paint);},1000/15);
  }
  function start(){if(raf===null&&frameTimer===null&&!document.hidden){last=-Infinity;paint(performance.now());}}
  const observer=new IntersectionObserver(entries=>{
    for(const entry of entries)entry.target.classList.toggle('presentation-motion-paused',!entry.isIntersecting);
    start();
  });
  function sync() {
    for(const node of tracked)if(!node.isConnected){observer.unobserve(node);tracked.delete(node);}
    for(const node of document.querySelectorAll('.service-card,.model-card,.display-row,.presentation-logo-shell'))if(!tracked.has(node)){
      node.classList.add('presentation-motion-paused');tracked.add(node);observer.observe(node);
    }
    effects=[...document.querySelectorAll('[data-presentation-motion]')];start();
  }
  function visibility(){
    document.body.classList.toggle('presentation-page-hidden',document.hidden);
    clearInterval(timer);timer=null;cancelAnimationFrame(raf);clearTimeout(frameTimer);raf=null;frameTimer=null;
    if(!document.hidden){refreshCountdowns();timer=setInterval(refreshCountdowns,1000);start();}
  }
  new MutationObserver(sync).observe(document.querySelector('#services'),{childList:true,subtree:true,attributes:true,attributeFilter:['data-activity-state']});
  for(const id of ['models','catalog_display_models'])new MutationObserver(sync).observe(document.getElementById(id),{childList:true});
  new MutationObserver(sync).observe(document.querySelector('#emp_logo'),{attributes:true,attributeFilter:['class']});
  document.addEventListener('visibilitychange',visibility);motion.addEventListener('change',()=>{cancelAnimationFrame(raf);clearTimeout(frameTimer);raf=null;frameTimer=null;start();});
  sync();visibility();return sync;
}
