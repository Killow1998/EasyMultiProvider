// One temporary sprite sheet: paint the wave once, then move its texture.
// Geometry/palette changes rebuild it; hover frames only change transform.
function installInteractions() {
  const layer=document.createElement('div');
  layer.className='presentation-hover-feedback'; layer.setAttribute('aria-hidden','true'); layer.hidden=true;
  document.body.append(layer);
  const surface=document.createElement('canvas');surface.setAttribute('aria-hidden','true');surface.width=surface.height=0;
  const motion=matchMedia('(prefers-reduced-motion:reduce)');
  const selector='button:not(:disabled),a.repo-link,.presentation-dot-custom';
  let target=null, canvas=null, dots=[], geometry='', width=0, height=0, frames=0, raf=null, frameTimer=null, releaseTimer=null, paletteKey='', lastFrame=-1;
  const observer=new ResizeObserver(draw);
  function release() {
    clearTimeout(releaseTimer);releaseTimer=null;
    surface.width=surface.height=0;canvas=null;dots=[];geometry='';paletteKey='';layer.replaceChildren();
  }
  function hide(immediate=false) {
    observer.disconnect();cancelAnimationFrame(raf);clearTimeout(frameTimer);clearTimeout(releaseTimer);
    raf=null;frameTimer=null;target=null;layer.hidden=true;
    if(immediate)release();else releaseTimer=setTimeout(release,8000);
  }
  function colors() {
    const style=getComputedStyle(layer);
    return [style.color,style.getPropertyValue('--presentation-dot-secondary').trim()||'#9473c6',style.getPropertyValue('--presentation-dot-third').trim()||'#299c9f'];
  }
  function tick(time) {
    raf=null; frameTimer=null;
    if(document.hidden)return hide(true);
    if(!target?.isConnected)return hide();
    const frame=motion.matches ? 0 : Math.floor(time/ (1600/frames))%frames;
    if (frame!==lastFrame) {
      canvas.style.transform=`translateY(-${frame*100/frames}%)`; lastFrame=frame;
    }
    if (!motion.matches) frameTimer=setTimeout(()=>{frameTimer=null;raf=requestAnimationFrame(tick);},1000/15);
  }
  function paint() {
    cancelAnimationFrame(raf); clearTimeout(frameTimer); raf=null; frameTimer=null;
    if(document.hidden)return hide(true);
    if(!target?.isConnected)return hide();
    if (!dots.length) return;
    const palette=colors();paletteKey=palette.join('/');
    frames=motion.matches ? 1 : 24;
    const scale=Math.min(devicePixelRatio||1,2), rowHeight=Math.ceil(height*scale), pixelWidth=Math.ceil(width*scale);
    canvas=surface;
    canvas.width=pixelWidth; canvas.height=rowHeight*frames;
    canvas.style.height=`${rowHeight/scale*frames}px`;
    const context=canvas.getContext('2d'); context.scale(scale,scale);
    for(let frame=0;frame<frames;frame++) {
      for(let i=0;i<dots.length;i++) {
        const dot=dots[i], wave=motion.matches ? .6 : (1-Math.cos((frame/frames+i/dots.length)*Math.PI*2))/2;
        context.globalAlpha=.4+.6*wave; context.fillStyle=palette[i%3]; context.beginPath();
        context.arc(dot.x+dot.dx*wave,dot.y+dot.dy*wave+frame*rowHeight/scale,.8+.5*wave,0,Math.PI*2); context.fill();
      }
    }
    layer.replaceChildren(canvas); lastFrame=-1; tick(performance.now());
  }
  function draw() {
    if(!target?.isConnected || document.hidden) return hide();
    const rect=target.getBoundingClientRect();
    if(!rect.width || !rect.height || rect.bottom<0 || rect.top>innerHeight || rect.right<0 || rect.left>innerWidth) return hide();
    const pad=4, radius=Math.min(parseFloat(getComputedStyle(target).borderRadius)||0,rect.height/2)+1;
    width=rect.width+pad*2; height=rect.height+pad*2;
    // Pixel-align the clip with each atlas row, including fractional zoom sizes.
    const scale=Math.min(devicePixelRatio||1,2); height=Math.ceil(height*scale)/scale;
    layer.style.cssText=`left:${rect.left-pad}px;top:${rect.top-pad}px;width:${width}px;height:${height}px`;
    layer.hidden=false;
    const key=`${width}/${height}/${radius}/${scale}`;
    if(key===geometry&&canvas&&paletteKey===colors().join('/')&&frames===(motion.matches?1:24)){if(raf===null&&frameTimer===null)tick(performance.now());return;}
    geometry=key;
    const svg=document.createElementNS('http://www.w3.org/2000/svg','svg');
    svg.innerHTML=`<rect x="3" y="3" width="${width-6}" height="${height-6}" rx="${radius}"/>`;
    const outline=svg.firstChild, length=outline.getTotalLength(), count=Math.max(12,Math.round(length/6));
    dots=Array.from({length:count},(_,i)=>{
      const point=outline.getPointAtLength(i*length/count);
      return {x:point.x,y:point.y,dx:(point.x-width/2)/(width/2)*1.2,dy:(point.y-height/2)/(height/2)*1.2};
    });
    paint();
  }
  function show(control) {
    if(!control || control===target) return;
    hide();clearTimeout(releaseTimer);releaseTimer=null;target=control;observer.observe(target);draw();
  }
  const focused=()=>document.activeElement?.matches(':focus-visible') ? document.activeElement.closest(selector) : null;
  document.addEventListener('pointerover',event=>show(event.target.closest(selector)));
  document.addEventListener('pointerout',event=>{if(target?.contains(event.target)&&!target.contains(event.relatedTarget)){hide();show(focused());}});
  document.addEventListener('focusin',()=>{const control=focused();if(control)show(control);});
  document.addEventListener('focusout',()=>{if(!target?.matches(':hover'))hide();});
  document.addEventListener('visibilitychange',()=>{if(document.hidden)hide(true);});
  motion.addEventListener('change',paint);
  window.addEventListener('presentation-dot-color-change',paint);
  window.addEventListener('scroll',draw,true); window.addEventListener('resize',draw); window.addEventListener('blur',()=>hide(true));
}
