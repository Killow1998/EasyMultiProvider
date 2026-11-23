function releaseHistoryText(info,locale,tr) {
  const releases=Array.isArray(info.release_history) && info.release_history.length ? info.release_history : [{version:info.latest_version,body:info.release_notes}];
  const groups={improvements:[],fixes:[]}, other=[];
  for(const release of releases) {
    const text=localizedReleaseNotes(release.body,release.version,locale);
    if(!text)continue;
    if(releases.length===1)return text+(info.release_history_complete===false?'\n\n'+tr('transfers.release_history_more'):'');
    const lines=text.split('\n');
    if(lines.some(line=>line.trim() && !/^[-*]\s+/.test(line))) {
      other.push(`### ${release.version}\n${text}`);continue;
    }
    for(const line of lines.filter(line=>line.trim())) {
      const item=line.replace(/^[-*]\s+/,''), fixed=/^(修复|修正|解决|fix(?:ed|es)?\b|correct(?:ed)?\b|resolve(?:d)?\b)/i.test(item);
      groups[fixed?'fixes':'improvements'].push(`- [${release.version}] ${item}`);
    }
  }
  let text=[...Object.entries(groups).filter(([,items])=>items.length).map(([key,items])=>`### ${tr('transfers.release_'+key)}\n${items.join('\n')}`),...other].join('\n\n');
  if(info.release_history_complete===false)text+='\n\n'+tr('transfers.release_history_more');
  return text;
}

const renderedReleaseNotes = new WeakMap();
function renderReleaseNotes(container, markdown) {
  if (renderedReleaseNotes.get(container) === markdown) return;
  const doc = container.ownerDocument;
  const fragment = doc.createDocumentFragment();
  function inline(parent, text) {
    const tokens = /`([^`\n]+)`|\*\*(.+?)\*\*|\[([^\]\n]+)\]\((https?:\/\/[^\s()]+)\)/gi;
    let end = 0;
    for (const match of text.matchAll(tokens)) {
      parent.append(text.slice(end, match.index));
      const [, code, bold, label, href] = match;
      const el = doc.createElement(code !== undefined ? 'code' : bold !== undefined ? 'strong' : 'a');
      if (href) {
        el.href = href;
        el.target = '_blank';
        el.rel = 'noopener noreferrer';
      }
      if (bold !== undefined) inline(el, bold);
      else el.textContent = code ?? label;
      parent.append(el);
      end = match.index + match[0].length;
    }
    parent.append(text.slice(end));
  }
  let block;
  for (const raw of markdown.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) { block = null; continue; }
    const heading = line.match(/^(#{1,6})\s+(.+)$/);
    if (heading) {
      const el = doc.createElement('h' + heading[1].length);
      inline(el, heading[2]);
      fragment.append(el);
      block = null;
      continue;
    }
    const item = line.match(/^(?:[-*+]|(\d+)[.)])\s+(.+)$/);
    const tag = item ? (item[1] ? 'ol' : 'ul') : 'p';
    if (block?.localName !== tag) {
      block = doc.createElement(tag);
      if (tag === 'ol') block.start = Number(item[1]);
      fragment.append(block);
    }
    if (item) {
      const li = doc.createElement('li');
      inline(li, item[2]);
      block.append(li);
    } else {
      if (block.hasChildNodes()) block.append(' ');
      inline(block, line);
    }
  }
  container.replaceChildren(fragment);
  renderedReleaseNotes.set(container, markdown);
}

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
  if(String(version).replace(/^v/,'')==='0.12.18' && normalized(text)===normalized(publishedV01218BodyMatcher))return chinese ? legacyChinese : releaseEnglish;
  if(!chinese)return /[\u3400-\u9fff]/.test(text)?'':text;
  if(/[\u3400-\u9fff]/.test(text))return text;
  return '';
}
const publishedV01218BodyMatcher="- Ship the accepted shadow presentation in the official Web UI: service artwork owns activity and call details, with brand colors, bounded halos and error bubbles.\n- Keep bars as the formal default; add mirrored compact quota rings, persistent style selection, two-unit countdowns and the existing 233% / 1m111s unlimited display.\n- Combine performance and usage into Statistics with common filters, Token / USD charts, service/model colors and click-through request outcomes.\n- Add filtered accounting chart series to the existing usage API. Totals and chart groups use the same reconciled ledger; service, model, session and outcome filters apply together.\n- Share the official presentation with the read-only developer preview. Preview command isolation and demos remain separate.\n- Use configurable dotted hover/focus feedback across controls, align language/theme selectors, and pause effects outside the viewport or in the background.";
const legacyChinese="- 改进 Web 界面：服务图标展示调用状态与详情，配合品牌颜色、光晕及错误气泡。\n- 正式版默认横条；新增左右对称的紧凑额度圆环、样式记忆、双单位倒计时，并保留无 5h 限制时的 233% / 1m111s 显示。\n- 将性能与用量合并为统计，统一筛选，支持 Token／USD 图表、按服务／模型区分颜色及查看调用结果。\n- 用量 API 支持筛选后的图表序列；总量与图表共用去重后的记录，服务、模型、会话及调用状态筛选同步生效。\n- 按钮使用可配置的点阵悬停／焦点反馈，对齐语言和外观选择器，并暂停不可见及后台动效。";

const releaseEnglish="- Improve the Web UI: service artwork owns activity and call details, with brand colors, bounded halos and error bubbles.\n- Keep bars as the formal default; add mirrored compact quota rings, persistent style selection, two-unit countdowns and the existing 233% / 1m111s unlimited display.\n- Combine performance and usage into Statistics with common filters, Token / USD charts, service/model colors and click-through request outcomes.\n- Add filtered accounting chart series to the existing usage API. Totals and chart groups use the same reconciled ledger; service, model, session and outcome filters apply together.\n- Use configurable dotted hover/focus feedback across controls, align language/theme selectors, and pause effects outside the viewport or in the background.";
