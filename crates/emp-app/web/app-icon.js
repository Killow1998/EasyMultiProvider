function createAppIcon({getState,api,openModal,$,esc,tr,notice,presets}) {
  const names={emp:()=>tr('icons.emp'),codex:()=> 'Codex',...Object.fromEntries(Object.entries(presets).filter(([id])=>id!=='chatgpt').map(([id,preset])=>[id,()=>preset.name]))};
  const neutral=new Set(['inception','xai','moonshot']);
  const current=()=>getState().app_icon || {preset:'emp',monochrome:false,image_url:'/assets/emp-icon.png'};
  function apply() {
    const icon=current();
    if($('app_favicon').getAttribute('href')!==icon.image_url)$('app_favicon').href=icon.image_url;
  }
  function source(preset,monochrome,custom) {
    if(preset==='emp')return monochrome?'/assets/emp-icon-mono.svg':'/assets/emp-icon.png';
    if(preset==='custom')return custom;
    const colorId={anthropic:'claudecode'}[preset] || preset;
    const text=(monochrome?SERVICE_BRAND_ICONS[preset]:BRAND_COLOR_ICONS[colorId] || SERVICE_BRAND_ICONS[preset]);
    const svg=new DOMParser().parseFromString(text,'image/svg+xml').documentElement;
    svg.setAttribute('width','512');svg.setAttribute('height','512');svg.style.color=!monochrome && preset==='openai'?'#10a37f':'#111111';
    return 'data:image/svg+xml;charset=utf-8,'+encodeURIComponent(new XMLSerializer().serializeToString(svg));
  }
  async function rasterize(url,monochrome=false) {
    const image=new Image();image.src=url;await image.decode();
    if(!image.naturalWidth || image.naturalWidth!==image.naturalHeight)throw new Error(tr('icons.square_required'));
    const canvas=document.createElement('canvas');canvas.width=canvas.height=512;
    const context=canvas.getContext('2d');context.drawImage(image,0,0,512,512);
    if(monochrome){const frame=context.getImageData(0,0,512,512);for(let i=0;i<frame.data.length;i+=4)frame.data[i]=frame.data[i+1]=frame.data[i+2]=0;context.putImageData(frame,0,0);}
    return canvas.toDataURL('image/png').split(',')[1];
  }
  function open(back) {
    const saved=current();let selected=saved.preset,monochrome=saved.monochrome,custom=saved.preset==='custom'?saved.source_url:null;
    const title=id=>id==='custom'?tr('icons.custom'):names[id]?.() || id;
    const choices=Object.keys(names).map(id=>`<button type="button" class="secondary app-icon-choice" data-app-icon="${id}" aria-pressed="false"><img class="${neutral.has(id)?'app-icon-neutral':''}" src="${esc(source(id,false,null))}" width="36" height="36" alt=""><span>${esc(id==='emp'?tr('icons.default'):title(id))}</span></button>`).join('');
    const styles=['color','mono'].map(mode=>`<button type="button" class="secondary" data-icon-style="${mode}" aria-pressed="false">${tr('icons.'+mode)}</button>`).join('');
    openModal(tr('icons.title'),`<div class="app-icon-heading"><img id="app_icon_preview" width="64" height="64" alt=""><div><strong id="app_icon_name"></strong><p class="muted">${tr('icons.applies_to')}</p></div></div>
      <div class="app-icon-grid">${choices}<button type="button" class="secondary app-icon-choice" data-app-icon="custom" aria-pressed="false"><span class="app-icon-upload" aria-hidden="true">＋</span><span>${tr('icons.custom')}</span></button></div>
      <div class="app-icon-options"><div class="quota-display-choice" role="group" aria-label="${tr('icons.style')}">${styles}</div><button type="button" class="secondary" id="app_icon_import">${tr('icons.import_svg')}</button><input id="app_icon_file" type="file" accept=".svg,image/svg+xml" hidden></div>
      <p class="muted">${tr('icons.svg_hint')}</p>`,tr('icons.save'),async()=>{
      const chosen=selected,mono=monochrome;
      $('modal_submit').disabled=true;
      try {
        const payload={preset:chosen,monochrome:mono};
        if(chosen!=='emp' || mono)payload.png=await rasterize(source(chosen,mono,custom),mono);
        if(chosen==='custom')payload.original_png=await rasterize(custom);
        const result=await api('/api/app-icon',{method:'POST',body:JSON.stringify(payload)});
        getState().app_icon=result.app_icon;apply();
        notice(tr(result.preview?'icons.preview_saved':result.launcher_updated?'icons.saved':'icons.launcher_failed'),!result.preview && !result.launcher_updated);
      } catch(error) {$('modal_status').textContent=error.payload?.error?.code?tr('icons.'+error.payload.error.code):error.message;}
      finally {$('modal_submit').disabled=false;}
    },null,{back,reopen:()=>open(back)});
    const paint=()=>{
      $('app_icon_preview').src=source(selected,monochrome,custom);
      $('app_icon_preview').classList.toggle('app-icon-neutral',neutral.has(selected));
      $('app_icon_preview').dataset.monochrome=String(monochrome);
      $('app_icon_name').textContent=title(selected);
      $('modal_body').querySelectorAll('[data-app-icon]').forEach(button=>button.setAttribute('aria-pressed',String(button.dataset.appIcon===selected)));
      $('modal_body').querySelectorAll('[data-icon-style]').forEach(button=>button.setAttribute('aria-pressed',String((button.dataset.iconStyle==='mono')===monochrome)));
    };
    const file=$('app_icon_file');
    $('modal_body').querySelectorAll('[data-app-icon]').forEach(button=>button.onclick=()=>{
      if(button.dataset.appIcon==='custom' && !custom){file.click();return;}
      selected=button.dataset.appIcon;paint();
    });
    $('modal_body').querySelectorAll('[data-icon-style]').forEach(button=>button.onclick=()=>{monochrome=button.dataset.iconStyle==='mono';paint();});
    $('app_icon_import').onclick=()=>file.click();
    file.onchange=async()=>{
      const upload=file.files[0];if(!upload)return;
      try {
        if(upload.size>1024*1024)throw new Error(tr('icons.svg_hint'));
        const text=await upload.text(),document=new DOMParser().parseFromString(text,'image/svg+xml'),svg=document.documentElement;
        const viewBox=svg.getAttribute('viewBox')?.trim().split(/[\s,]+/).map(Number);
        if(svg.localName!=='svg' || document.querySelector('parsererror') || viewBox && (viewBox.length!==4 || !(viewBox[2]>0) || viewBox[2]!==viewBox[3]))throw new Error(tr('icons.square_required'));
        const url='data:image/svg+xml;charset=utf-8,'+encodeURIComponent(text);
        custom='data:image/png;base64,'+await rasterize(url);selected='custom';$('modal_status').textContent='';paint();
      } catch(error){$('modal_status').textContent=error.message;}
      file.value='';
    };
    paint();
  }
  return {apply,open};
}
