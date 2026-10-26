// Combine maintained notes without translating or dropping migration instructions.
function releaseHistoryText(info,locale,tr) {
  const releases=Array.isArray(info.release_history) && info.release_history.length ? info.release_history : [{version:info.latest_version,body:info.release_notes}];
  const groups={improvements:[],fixes:[]}, other=[];
  for(const release of releases) {
    const text=localizedReleaseNotes(release.body,release.version,locale);
    if(!text)continue;
    if(releases.length===1)return text+(info.release_history_complete===false?'\n\n'+tr('transfers.release_history_more'):'');
    const lines=text.split('\n');
    // Keep rich or nested notes intact; only flat bullet lists can be regrouped safely.
    if(lines.some(line=>line.trim() && !/^[-*]\s+/.test(line))) {
      other.push(`${release.version}\n${text}`);continue;
    }
    for(const line of lines.filter(line=>line.trim())) {
      const item=line.replace(/^[-*]\s+/,''), fixed=/^(修复|修正|解决|fix(?:ed|es)?\b|correct(?:ed)?\b|resolve(?:d)?\b)/i.test(item);
      groups[fixed?'fixes':'improvements'].push(`- [${release.version}] ${item}`);
    }
  }
  let text=[...Object.entries(groups).filter(([,items])=>items.length).map(([key,items])=>`${tr('transfers.release_'+key)}\n${items.join('\n')}`),...other].join('\n\n');
  if(info.release_history_complete===false)text+='\n\n'+tr('transfers.release_history_more');
  return text;
}

// Prefer maintained language sections; never substitute a machine translation.
function localizedReleaseNotes(body,version,locale) {
  const text=String(body||'').trim(), chinese=locale!=='en';
  const lines=text.split('\n'), sections=new Map();let current=null;
  for(const line of lines){
    const heading=line.match(/^#{1,6}\s+(中文|简体中文|中文更新说明|Chinese|English|English release notes|英文)\s*$/i);
    if(heading){current=/english|英文/i.test(heading[1])?'en':'zh';sections.set(current,[]);}
    else if(current)sections.get(current).push(line);
  }
  if(sections.size)return (sections.get(chinese?'zh':'en')||[]).join('\n').trim();
  const normalized=value=>value.replace(/\s+/g,' ').trim();
  if(String(version).replace(/^v/,'')==='0.12.18' && normalized(text)===normalized(legacyEnglish))return chinese ? legacyChinese : releaseEnglish;
  if(!chinese)return /[\u3400-\u9fff]/.test(text)?'':text;
  if(/[\u3400-\u9fff]/.test(text))return text;
  return '';
}
// Exact matcher for the previously published body; it is never displayed directly.
const legacyEnglish="- Ship the accepted shadow presentation in the official Web UI: service artwork owns activity and call details, with brand colors, bounded halos and error bubbles.\n- Keep bars as the formal default; add mirrored compact quota rings, persistent style selection, two-unit countdowns and the existing 233% / 1m111s unlimited display.\n- Combine performance and usage into Statistics with common filters, Token / USD charts, service/model colors and click-through request outcomes.\n- Add filtered accounting chart series to the existing usage API. Totals and chart groups use the same reconciled ledger; service, model, session and outcome filters apply together.\n- Share the official presentation with the read-only developer preview. Preview command isolation and demos remain separate.\n- Use configurable dotted hover/focus feedback across controls, align language/theme selectors, and pause effects outside the viewport or in the background.";
const legacyChinese="- 改进 Web 界面：服务图标展示调用状态与详情，配合品牌颜色、光晕及错误气泡。\n- 正式版默认横条；新增左右对称的紧凑额度圆环、样式记忆、双单位倒计时，并保留无 5h 限制时的 233% / 1m111s 显示。\n- 将性能与用量合并为统计，统一筛选，支持 Token／USD 图表、按服务／模型区分颜色及查看调用结果。\n- 用量 API 支持筛选后的图表序列；总量与图表共用去重后的记录，服务、模型、会话及调用状态筛选同步生效。\n- 按钮使用可配置的点阵悬停／焦点反馈，对齐语言和外观选择器，并暂停不可见及后台动效。";

const releaseEnglish="- Improve the Web UI: service artwork owns activity and call details, with brand colors, bounded halos and error bubbles.\n- Keep bars as the formal default; add mirrored compact quota rings, persistent style selection, two-unit countdowns and the existing 233% / 1m111s unlimited display.\n- Combine performance and usage into Statistics with common filters, Token / USD charts, service/model colors and click-through request outcomes.\n- Add filtered accounting chart series to the existing usage API. Totals and chart groups use the same reconciled ledger; service, model, session and outcome filters apply together.\n- Use configurable dotted hover/focus feedback across controls, align language/theme selectors, and pause effects outside the viewport or in the background.";
