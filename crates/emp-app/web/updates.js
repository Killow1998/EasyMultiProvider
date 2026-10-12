// Update metadata, polling and presentation have one cancellable window owner.
function createUpdates({api,$,tr,t,esc,openModal,notice,getLanguage,localizedReleaseNotes,visible,openLink,copyText,isPreview}) {
let updatePollTimer = null;
let updatePollGeneration = 0;
let latestUpdateInfo = null;
let updateController=null;
const pendingStates=['checking','downloading','verifying','waiting','installing'];
function stopUpdatePoll() { updatePollGeneration++;updateController?.abort();updateController=null;if (updatePollTimer) clearTimeout(updatePollTimer); updatePollTimer = null; }
function updateErrorLabel(code) {
  const messages = {
    directory_not_writable:tr('安装目录没有写权限，请将 EMP 移到可写目录后重试。','The installation directory is not writable. Move EMP to a writable folder and retry.'),
    package_query_failed:tr('无法确认系统安装包状态。请先在系统包管理器中检查 EMP。','Could not confirm the installed package state. Check EMP in the system package manager first.'),
    requests_busy:tr('当前请求尚未结束，已取消安装，请稍后重试。','Requests are still running. Installation was cancelled; try again later.'),
    checksum_missing:tr('新版本缺少文件校验信息，暂时无法自动更新。','The release has no file checksum; automatic update is unavailable.'),
    checksum_mismatch:tr('下载文件校验失败，请重试。','Download verification failed. Please retry.'),
    package_missing:tr('新版本尚未提供此平台的安装包。','The release does not yet include a package for this platform.'),
    unsupported_installation:tr('当前安装方式不支持自动更新。','This installation does not support automatic updates.'),
    system_install_manual:tr('系统级 Linux 安装不能在运行中覆盖。请按迁移说明改为用户级安装。','A system Linux installation cannot be replaced while running. Follow the migration steps to install for your user.'),
    unsupported_platform:tr('此平台暂不支持自动更新。','Automatic updates are not available for this platform.'),
    install_rolled_back:tr('新版本未能启动，已恢复旧版本。','The new version could not start. The previous version has been restored.'),
  };
  return messages[code] || tr('更新未完成。请稍后重新检查更新；如果仍然失败，可从官方发布页下载安装包。','The update did not finish. Check again later; if it still fails, download the installer from the official release page.');
}
function updateFailureAdvice(failure) {
  const codeAdvice = {
    checksum_mismatch:tr('安装包内容与发布信息不符，可能下载损坏。请重新下载；若仍失败，请检查代理或网络缓存。','The package does not match the release, possibly because the download was corrupted. Download again; if it persists, check your proxy or network cache.'),
    invalid_release:tr('收到的版本信息格式不正确，可能下载服务或代理返回了其他页面。请检查网络后重新检查更新。','The release information is invalid; the service or proxy may have returned a different page. Check the network and check for updates again.'),
    invalid_package_size:tr('安装包大小不符合发布信息。请重新检查更新并下载官方安装包。','The package size does not match the release. Check for updates again and download the official installer.'),
    version_mismatch:tr('安装包中的版本与发布信息不符。请重新检查更新并下载官方安装包。','The package version does not match the release. Check for updates again and download the official installer.'),
    invalid_download_url:tr('安装包链接未通过安全检查。请重新检查更新，或从官方发布页下载安装包。','The package link failed the security check. Check for updates again or download from the official release page.'),
  };
  if (codeAdvice[failure.error]) return codeAdvice[failure.error];
  if (failure.stage === 'parse_release' && failure.error === 'update_failed') return codeAdvice.invalid_release;
  const networkStages = ['check_release','read_release','download_package'];
  const reasonAdvice = {
    incomplete_download:tr('安装包未下载完整，可能网络或代理连接中途断开。请检查连接后重新检查更新。','The package download was incomplete; the network or proxy may have disconnected. Check the connection and check for updates again.'),
    timeout:tr('连接超时，可能网络较慢、代理不可用或下载服务暂时无响应。请检查网络与代理后重试。','The connection timed out. The network may be slow, the proxy unavailable, or the download service unresponsive. Check the network and proxy, then retry.'),
    PermissionDenied:tr('无法写入安装文件。请确认安装目录可写，并检查系统防护是否阻止 EMP 更新。','The installation files could not be written. Check folder permissions and whether system protection is blocking the update.'),
    StorageFull:tr('磁盘空间不足。请释放安装目录所在磁盘的空间后重试。','The disk is full. Free space on the installation drive and retry.'),
  };
  if (reasonAdvice[failure.reason]) return reasonAdvice[failure.reason];
  if (networkStages.includes(failure.stage)) {
    if ([10054,104,54].includes(failure.os_error)) return tr('连接被中途断开，可能网络不稳定、代理断线或安全软件拦截。请检查网络与代理后重试。','The connection was reset. Possible causes include an unstable network, a disconnected proxy, or security software blocking the connection. Check the network and proxy, then retry.');
    if ([403,429].includes(failure.http_status)) return tr('下载请求被拒绝，可能触发访问限制或请求过于频繁。请稍后重试，并检查代理与网络访问规则。','The download request was rejected, possibly by access restrictions or rate limits. Try later and check proxy/network access rules.');
    if (failure.http_status === 404) return tr('安装包暂时不存在，可能尚未发布完整或已移除。请稍后重新检查更新。','The package is unavailable; publication may be incomplete or the asset removed. Check for updates again later.');
    if (failure.http_status >= 500) return tr('下载服务暂时不可用，也可能代理无法连接 GitHub。请稍后重试或检查代理。','The download service is temporarily unavailable, or the proxy cannot reach GitHub. Try later or check the proxy.');
    return tr('无法完成网络请求，可能网络、代理、DNS 或安全连接出现问题。请确认能访问官方发布页后重试。','The network request could not complete. Possible causes include network, proxy, DNS or secure-connection issues. Check access to the official release page and retry.');
  }
  if ([28,112].includes(failure.os_error)) return reasonAdvice.StorageFull;
  if ([5,13].includes(failure.os_error)) return reasonAdvice.PermissionDenied;
  const stageAdvice = {
    write_package:tr('无法保存安装包。请检查磁盘空间、目录权限及文件是否被占用。','The package could not be saved. Check disk space, folder permissions and whether the file is in use.'),
    sync_package:tr('安装包无法写入磁盘。请检查磁盘空间及存储设备状态。','The package could not be flushed to disk. Check disk space and storage health.'),
    extract_package:tr('安装包无法解包。请重新下载，并检查磁盘空间与系统防护提示。','The package could not be extracted. Download again and check disk space and system-protection messages.'),
    verify_version:tr('新版程序无法正常运行。请检查系统防护提示，并确认使用对应系统的官方安装包。','The new executable could not run correctly. Check system-protection messages and use the official package for your operating system.'),
    wait_worker:tr('更新程序未能就绪。请检查系统防护提示及安装目录权限后重试。','The updater did not become ready. Check system-protection messages and installation permissions, then retry.'),
  };
  if (stageAdvice[failure.stage]) return stageAdvice[failure.stage];
  return updateErrorLabel(failure.error);
}
function updateFailureLabel(info) {
  const failure = info.failure;
  const stages = {
    start_update:tr('启动更新任务','starting the update task'), check_release:tr('检查版本','checking for a release'), read_release:tr('读取版本信息','reading release information'), parse_release:tr('解析版本信息','validating release information'),
    prepare_install:tr('准备安装','preparing installation'), create_staging:tr('创建更新目录','creating the update folder'), download_package:tr('下载安装包','downloading the package'),
    write_package:tr('保存安装包','saving the package'), sync_package:tr('写入安装包','flushing the package'), verify_checksum:tr('校验安装包','verifying the checksum'), extract_package:tr('解包','extracting the package'),
    verify_version:tr('检查新版程序','checking the new executable'), prepare_worker:tr('准备更新程序','preparing the updater'), write_plan:tr('保存更新计划','saving the update plan'), drain_requests:tr('等待请求结束','waiting for requests'),
    start_worker:tr('启动更新程序','starting the updater'), wait_worker:tr('等待更新程序就绪','waiting for the updater'), shutdown_previous:tr('退出旧版','exiting the previous version'),
    replacing:tr('替换程序','replacing the executable'), starting:tr('启动新版','starting the new version'), waiting_for_startup:tr('等待新版启动','waiting for startup'),
  };
  if (!failure || (failure.error !== info.error && info.error !== 'install_rolled_back') || !stages[failure.stage]) return updateErrorLabel(info.error);
  const reasons = {timeout:tr('连接超时','connection timed out'), connection:tr('连接失败','connection failed'), PermissionDenied:tr('没有写入权限','permission denied'), StorageFull:tr('磁盘空间不足','disk full')};
  const detail = reasons[failure.reason] || (failure.http_status ? `HTTP ${failure.http_status}` : Number.isInteger(failure.os_error) ? tr(`系统错误 ${failure.os_error}`,`system error ${failure.os_error}`) : Number.isInteger(failure.exit_code) ? tr(`程序退出码 ${failure.exit_code}`,`exit code ${failure.exit_code}`) : '');
  const retries = Number.isInteger(failure.retry_count) && failure.retry_count > 0 ? tr(`已自动重试 ${failure.retry_count} 次。`, `Automatically retried ${failure.retry_count} times. `) : '';
  return retries + tr(`${stages[failure.stage]}时失败`, `Update failed while ${stages[failure.stage]}`) + (detail ? tr(`：${detail}`, `: ${detail}`) : '') + tr('。 ', '. ') + updateFailureAdvice(failure) + (info.error === 'install_rolled_back' ? tr(' 已恢复旧版。', ' The previous version has been restored.') : '');
}
function renderUpdateHint(info) {
  latestUpdateInfo = info;
  const button = $('check_update');
  const available = info.state === 'available' || (info.state === 'error' && info.latest_version && info.latest_version !== info.current_version);
  button?.classList.toggle('has-update', Boolean(available));
  if (button) {
    button.title = available ? tr(`有可用更新 v${info.latest_version}`, `Update available: v${info.latest_version}`) : t('check_update');
    button.setAttribute('aria-label', button.title);
  }
}
let updateHintBusy = false;
let updateHintCheckedAt = 0;
async function checkUpdateHint() {
  if (updateHintBusy || !visible() || Date.now() - updateHintCheckedAt < 6 * 60 * 60 * 1000) return;
  updateHintBusy = true;
  updateHintCheckedAt = Date.now();
  try {
    let info = await api('/api/updates');
    renderUpdateHint(info);
    if (['idle','current','no_release','error'].includes(info.state)) info = await api('/api/updates/check', {method:'POST', body:'{}'});
    // Only wait for this check to finish; no permanent background polling.
    for (let attempt = 0; info.state === 'checking' && attempt < 25; attempt++) {
      await new Promise(resolve => setTimeout(resolve, 1000));
      info = await api('/api/updates');
    }
    renderUpdateHint(info);
  } catch (_) {} finally { updateHintBusy = false; }
}
function renderUpdate(info) {
  renderUpdateHint(info);
  const labels = {
    idle:tr('检查可用版本。','Check for an available version.'), checking:tr('正在检查…','Checking…'),
    no_release:tr('暂无正式版更新。','No stable release is available.'), current:tr('当前已是最新版本。','You are up to date.'),
    available:tr(`发现新版本 v${info.latest_version}。`,`Version ${info.latest_version} is available.`),
    migration_required:tr('检测到旧的 Linux 系统级安装。请先退出 EMP 并恢复原生 Codex，再迁移到用户目录；现有配置会继续保留。','A legacy system-wide Linux installation was detected. Exit EMP and restore native Codex, then migrate to a user installation. Your existing configuration is preserved.'),
    downloading:tr(`正在后台下载 ${info.progress}%…`,`Downloading in the background: ${info.progress}%…`),
    verifying:tr('正在校验安装包…','Verifying the package…'), waiting:tr('下载完成，正在等待当前请求结束…','Download complete. Waiting for current requests to finish…'),
    installing:tr('正在安装并重启 EMP，完成后会打开新页面。','Installing and restarting EMP. A new page will open when ready.'),
    error:updateFailureLabel(info),
  };
  $('update_status').textContent = labels[info.state] || labels.idle;
  if (['checking','downloading'].includes(info.state) && info.retry_count > 0) $('update_status').textContent += tr(` 正在自动重试 ${info.retry_count}/${info.retry_limit}。`, ` Retrying automatically ${info.retry_count}/${info.retry_limit}.`);
  $('update_version').textContent = tr(`当前版本 v${info.current_version}`,`Current version: ${info.current_version}`);
  const notes=$('update_notes');
  if(notes)notes.textContent=localizedReleaseNotes(info.release_notes,info.latest_version,getLanguage()) || tr('此版本尚未提供中文更新说明，请查看官方发布页。','English release notes were not provided. See the official release page.');
  $('update_progress').hidden = !['downloading','verifying'].includes(info.state);
  $('update_progress').value = Number(info.progress) || 0;
  const install = $('update_install'); install.disabled=isPreview();if(isPreview())install.title=tr('影子页只查看更新说明；请在正式页面更新。','Preview only; install from the main page.'); install.hidden = info.state !== 'available' || !info.supported;
  const release = $('update_release'); release.hidden = !info.latest_version && info.manual_update !== 'linux_system_migration';
  const commands = $('update_commands'); commands.hidden = info.manual_update !== 'linux_system_migration';
  const manual = $('update_manual'); manual.hidden = info.manual_update !== 'linux_system_migration';
  if (info.state === 'available' && !info.supported) $('update_status').textContent += tr(' 请使用 EMP 安装包启动后更新。',' Launch the packaged EMP app to update.');
}
function openLatestRelease() {
  const url = latestUpdateInfo?.release_url;
  if (url === 'https://github.com/Killow1998/EasyMultiProvider/releases/latest') openLink(url);
}
async function copyLinuxMigrationCommands() {
  const commands = `curl -fL -o EMP-linux-x86_64-install.sh https://github.com/Killow1998/EasyMultiProvider/releases/latest/download/EMP-linux-x86_64-install.sh\nsh EMP-linux-x86_64-install.sh`;
  try { await copyText(commands); notice(tr('迁移命令已复制。','Migration commands copied.')); }
  catch (_) { $('update_status').textContent = commands; }
}
async function pollUpdate(generation) {
  if(generation!==updatePollGeneration)return;
  try {
    const info = await api('/api/updates',{signal:updateController?.signal});
    if (generation !== updatePollGeneration) return;
    renderUpdate(info);
    if (pendingStates.includes(info.state)) updatePollTimer = setTimeout(() => pollUpdate(generation), 1000);
  } catch (_) {
    if (generation === updatePollGeneration) $('update_status').textContent = tr('连接已断开。若正在更新，请等待 EMP 重新打开页面。','Connection closed. If updating, wait for EMP to reopen the page.');
  }
}
async function requestUpdate(operation) {
  stopUpdatePoll(); const generation = updatePollGeneration;
  updateController=new AbortController();const controller=updateController;
  $('update_install').disabled = true;
  try { const info = await api('/api/updates/' + operation, {method:'POST', body:'{}',signal:controller.signal}); if (generation === updatePollGeneration) { renderUpdate(info); if(pendingStates.includes(info.state))pollUpdate(generation); } }
  catch (error) { if (generation === updatePollGeneration) $('update_status').textContent = error.message; }
  finally { if (generation === updatePollGeneration) $('update_install').disabled = isPreview(); }
}
async function openUpdate() {
  openModal(tr('更新 EMP','Update EMP'), `<p id="update_version" class="muted"></p><div id="update_status" role="status" aria-live="polite">${tr('正在检查…','Checking…')}</div><progress id="update_progress" class="update-progress" max="100" hidden></progress><div class="update-notes"><h3>${tr('更新内容','What changed')}</h3><div id="update_notes" class="update-notes-content">${tr('正在读取更新说明…','Loading release notes…')}</div></div><div id="update_manual" class="muted" hidden><p>${tr('建議：下載 Linux 雙語安裝腳本，退出目前 EMP 後執行。安裝器會詢問是否移除舊系統套件，並可遷移源碼目錄中的設定。','Recommended: download the bilingual Linux installer, exit EMP, then run it. The installer asks before removing a system package and can migrate configuration from a source checkout.')}</p><p>${tr('舊系統安裝的使用者設定會保留；來源設定也只會複製，不會刪除。','Existing system-install user settings are kept; source settings are copied and remain in place.')}</p></div><div class="modal-actions"><button id="update_release" type="button" class="secondary" hidden onclick="openLatestRelease()">${tr('打開最新 Release','Open latest Release')}</button><button id="update_commands" type="button" class="secondary" hidden onclick="copyLinuxMigrationCommands()">${tr('複製安裝命令','Copy installer command')}</button><button id="update_install" type="button" data-icon="update" hidden onclick="requestUpdate('install')">${tr('后台更新','Update in background')}</button></div>`, '', null);
  await requestUpdate('check');
}

return {open:openUpdate,stop:stopUpdatePoll,request:requestUpdate,render:renderUpdate,hint:renderUpdateHint,checkHint:checkUpdateHint,openRelease:openLatestRelease,copyCommands:copyLinuxMigrationCommands,info:()=>latestUpdateInfo};
}
