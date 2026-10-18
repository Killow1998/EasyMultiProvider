// Update metadata, polling and presentation have one cancellable window owner.
function createUpdates({api,$,tr,t,esc,openModal,notice,getLanguage,localizedReleaseNotes,visible,openLink,copyText,isPreview,reload}) {
let updatePollTimer = null;
let updatePollGeneration = 0;
let latestUpdateInfo = null;
let updateController=null;
let installingFrom=null, reconnectDeadline=0;
const pendingStates=['checking','downloading','verifying','waiting','installing'];
function stopUpdatePoll() { updatePollGeneration++;updateController?.abort();updateController=null;if (updatePollTimer) clearTimeout(updatePollTimer); updatePollTimer = null;installingFrom=null;reconnectDeadline=0; }
function updateErrorLabel(code) {
  const messages = {
    directory_not_writable:tr("transfers.the_installation_directory_is_not_writable_move_emp_to"),
    package_query_failed:tr("transfers.could_not_confirm_the_installed_package_state_check_emp"),
    requests_busy:tr("transfers.requests_are_still_running_installation_was_cancelled_try_again"),
    checksum_missing:tr("transfers.the_release_has_no_file_checksum_automatic_update_is"),
    checksum_mismatch:tr("transfers.download_verification_failed_please_retry"),
    package_missing:tr("transfers.the_release_does_not_yet_include_a_package_for"),
    unsupported_installation:tr("transfers.this_installation_does_not_support_automatic_updates"),
    system_install_manual:tr("transfers.a_system_linux_installation_cannot_be_replaced_while_running"),
    unsupported_platform:tr("transfers.automatic_updates_are_not_available_for_this_platform"),
    install_rolled_back:tr("transfers.the_new_version_could_not_start_the_previous_version"),
  };
  return messages[code] || tr("transfers.the_update_did_not_finish_check_again_later_if");
}
function updateFailureAdvice(failure) {
  const codeAdvice = {
    checksum_mismatch:tr("transfers.the_package_does_not_match_the_release_possibly_because"),
    invalid_release:tr("transfers.the_release_information_is_invalid_the_service_or_proxy"),
    invalid_package_size:tr("transfers.the_package_size_does_not_match_the_release_check"),
    version_mismatch:tr("transfers.the_package_version_does_not_match_the_release_check"),
    invalid_download_url:tr("transfers.the_package_link_failed_the_security_check_check_for"),
  };
  if (codeAdvice[failure.error]) return codeAdvice[failure.error];
  if (failure.stage === 'parse_release' && failure.error === 'update_failed') return codeAdvice.invalid_release;
  const networkStages = ['check_release','read_release','download_package'];
  const reasonAdvice = {
    incomplete_download:tr("transfers.the_package_download_was_incomplete_the_network_or_proxy"),
    timeout:tr("transfers.the_connection_timed_out_the_network_may_be_slow"),
    PermissionDenied:tr("transfers.the_installation_files_could_not_be_written_check_folder"),
    StorageFull:tr("transfers.the_disk_is_full_free_space_on_the_installation"),
  };
  if (reasonAdvice[failure.reason]) return reasonAdvice[failure.reason];
  if (networkStages.includes(failure.stage)) {
    if ([10054,104,54].includes(failure.os_error)) return tr("transfers.the_connection_was_reset_possible_causes_include_an_unstable");
    if ([403,429].includes(failure.http_status)) return tr("transfers.the_download_request_was_rejected_possibly_by_access_restrictions");
    if (failure.http_status === 404) return tr("transfers.the_package_is_unavailable_publication_may_be_incomplete_or");
    if (failure.http_status >= 500) return tr("transfers.the_download_service_is_temporarily_unavailable_or_the_proxy");
    return tr("transfers.the_network_request_could_not_complete_possible_causes_include");
  }
  if ([28,112].includes(failure.os_error)) return reasonAdvice.StorageFull;
  if ([5,13].includes(failure.os_error)) return reasonAdvice.PermissionDenied;
  const stageAdvice = {
    write_package:tr("transfers.the_package_could_not_be_saved_check_disk_space"),
    sync_package:tr("transfers.the_package_could_not_be_flushed_to_disk_check"),
    extract_package:tr("transfers.the_package_could_not_be_extracted_download_again_and"),
    verify_version:tr("transfers.the_new_executable_could_not_run_correctly_check_system"),
    wait_worker:tr("transfers.the_updater_did_not_become_ready_check_system_protection"),
  };
  if (stageAdvice[failure.stage]) return stageAdvice[failure.stage];
  return updateErrorLabel(failure.error);
}
function updateFailureLabel(info) {
  const failure = info.failure;
  const stages = {
    start_update:tr("transfers.starting_the_update_task"), check_release:tr("transfers.checking_for_a_release"), read_release:tr("transfers.reading_release_information"), parse_release:tr("transfers.validating_release_information"),
    prepare_install:tr("transfers.preparing_installation"), create_staging:tr("transfers.creating_the_update_folder"), download_package:tr("transfers.downloading_the_package"),
    write_package:tr("transfers.saving_the_package"), sync_package:tr("transfers.flushing_the_package"), verify_checksum:tr("transfers.verifying_the_checksum"), extract_package:tr("transfers.extracting_the_package"),
    verify_version:tr("transfers.checking_the_new_executable"), prepare_worker:tr("transfers.preparing_the_updater"), write_plan:tr("transfers.saving_the_update_plan"), drain_requests:tr("transfers.waiting_for_requests"),
    start_worker:tr("transfers.starting_the_updater"), wait_worker:tr("transfers.waiting_for_the_updater"), shutdown_previous:tr("transfers.exiting_the_previous_version"),
    replacing:tr("transfers.replacing_the_executable"), starting:tr("transfers.starting_the_new_version"), waiting_for_startup:tr("transfers.waiting_for_startup"),
  };
  if (!failure || (failure.error !== info.error && info.error !== 'install_rolled_back') || !stages[failure.stage]) return updateErrorLabel(info.error);
  const reasons = {timeout:tr("transfers.connection_timed_out"), connection:tr("transfers.connection_failed"), PermissionDenied:tr("transfers.permission_denied"), StorageFull:tr("transfers.disk_full")};
  const detail = reasons[failure.reason] || (failure.http_status ? `HTTP ${failure.http_status}` : Number.isInteger(failure.os_error) ? tr("transfers.system_error", {osError:failure.os_error}) : Number.isInteger(failure.exit_code) ? tr("transfers.exit_code", {exitCode:failure.exit_code}) : '');
  const retries = Number.isInteger(failure.retry_count) && failure.retry_count > 0 ? tr("transfers.automatically_retried_times", {retryCount:failure.retry_count}) : '';
  return retries + tr("transfers.update_failed_while", {value:stages[failure.stage]}) + (detail ? tr("transfers.message", {detail:detail}) : '') + tr("transfers.message_2") + updateFailureAdvice(failure) + (info.error === 'install_rolled_back' ? tr("transfers.the_previous_version_has_been_restored") : '');
}
function renderUpdateHint(info) {
  latestUpdateInfo = info;
  const button = $('check_update');
  const available = info.state === 'available' || (info.state === 'error' && info.latest_version && info.latest_version !== info.current_version);
  button?.classList.toggle('has-update', Boolean(available));
  if (button) {
    button.title = available ? tr("transfers.update_available_v", {latestVersion:info.latest_version}) : t('check_update');
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
    idle:tr("transfers.check_for_an_available_version"), checking:tr("transfers.checking"),
    no_release:tr("transfers.no_stable_release_is_available"), current:tr("transfers.you_are_up_to_date"),
    available:tr("transfers.version_is_available", {latestVersion:info.latest_version}),
    migration_required:tr("transfers.a_legacy_system_wide_linux_installation_was_detected_exit"),
    downloading:tr("transfers.downloading_in_the_background", {progress:info.progress}),
    verifying:tr("transfers.verifying_the_package"), waiting:tr("transfers.download_complete_waiting_for_current_requests_to_finish"),
    installing:tr("transfers.installing_and_restarting_emp_this_page_will_refresh_when"),
    error:updateFailureLabel(info),
  };
  $('update_status').textContent = labels[info.state] || labels.idle;
  if (['checking','downloading'].includes(info.state) && info.retry_count > 0) $('update_status').textContent += tr("transfers.retrying_automatically", {retryCount:info.retry_count, retryLimit:info.retry_limit});
  $('update_version').textContent = info.latest_version && info.latest_version !== info.current_version
    ? `v${info.current_version} → v${info.latest_version}`
    : tr("transfers.current_version_v", {currentVersion:info.current_version});
  const notes=$('update_notes');
  if(notes)notes.textContent=localizedReleaseNotes(info.release_notes,info.latest_version,getLanguage()) || tr(!info.latest_version && ['idle','checking'].includes(info.state) ? "transfers.loading_release_notes" : "transfers.english_release_notes_were_not_provided_see_the_official");
  $('update_progress').hidden = !['downloading','verifying'].includes(info.state);
  $('update_progress').value = Number(info.progress) || 0;
  const install = $('update_install'); install.disabled=isPreview();if(isPreview())install.title=tr("transfers.preview_only_install_from_the_main_page"); install.hidden = info.state !== 'available' || !info.supported;
  const release = $('update_release'); release.hidden = !info.latest_version && info.manual_update !== 'linux_system_migration';
  const commands = $('update_commands'); commands.hidden = info.manual_update !== 'linux_system_migration';
  const manual = $('update_manual'); manual.hidden = info.manual_update !== 'linux_system_migration';
  if (info.state === 'available' && !info.supported) $('update_status').textContent += tr("transfers.launch_the_packaged_emp_app_to_update");
}
function openLatestRelease() {
  const url = latestUpdateInfo?.release_url;
  if (url === 'https://github.com/Killow1998/EasyMultiProvider/releases/latest') openLink(url);
}
async function copyLinuxMigrationCommands() {
  const commands = `curl -fL -o EMP-linux-x86_64-install.sh https://github.com/Killow1998/EasyMultiProvider/releases/latest/download/EMP-linux-x86_64-install.sh\nsh EMP-linux-x86_64-install.sh`;
  try { await copyText(commands); notice(tr("transfers.migration_commands_copied")); }
  catch (_) { $('update_status').textContent = commands; }
}
async function pollUpdate(generation) {
  if(generation!==updatePollGeneration)return;
  try {
    const info = await api('/api/updates',{signal:updateController?.signal});
    if (generation !== updatePollGeneration) return;
    reconnectDeadline=0;
    if (installingFrom && info.current_version && info.current_version !== installingFrom) { stopUpdatePoll();reload();return; }
    renderUpdate(info);
    if (pendingStates.includes(info.state)) updatePollTimer = setTimeout(() => pollUpdate(generation), 1000);
    else installingFrom=null;
  } catch (error) {
    if (generation === updatePollGeneration) reconnectUpdate(error,generation);
  }
}
function reconnectUpdate(error,generation) {
  if (installingFrom && !error.status) {
    reconnectDeadline ||= Date.now()+180000;
    if (Date.now()<reconnectDeadline) {
      $('update_status').textContent=tr("transfers.waiting_for_emp_to_restart_this_page_will_refresh");
      updatePollTimer=setTimeout(()=>pollUpdate(generation),1000);return;
    }
  }
  installingFrom=null;
  $('update_status').textContent=error.status ? error.message : tr("transfers.cannot_connect_to_emp_yet_check_that_emp_is");
}
async function requestUpdate(operation) {
  stopUpdatePoll(); const generation = updatePollGeneration;
  if(operation==='install'&&!isPreview())installingFrom=latestUpdateInfo?.current_version||null;
  updateController=new AbortController();const controller=updateController;
  $('update_install').disabled = true;
  try { const info = await api('/api/updates/' + operation, {method:'POST', body:'{}',signal:controller.signal}); if (generation === updatePollGeneration) { renderUpdate(info); if(pendingStates.includes(info.state))pollUpdate(generation); } }
  catch (error) { if (generation === updatePollGeneration) reconnectUpdate(error,generation); }
  finally { if (generation === updatePollGeneration) $('update_install').disabled = isPreview(); }
}
async function openUpdate() {
  openModal(tr("transfers.update_emp"), `<p id="update_version" class="muted"></p><div id="update_status" role="status" aria-live="polite">${tr("transfers.checking")}</div><progress id="update_progress" class="update-progress" max="100" hidden></progress><div class="update-notes"><h3>${tr("transfers.what_changed")}</h3><div id="update_notes" class="update-notes-content">${tr("transfers.loading_release_notes")}</div></div><div id="update_manual" class="muted" hidden><p>${tr("transfers.recommended_download_the_bilingual_linux_installer_exit_emp_then")}</p><p>${tr("transfers.existing_system_install_user_settings_are_kept_source_settings")}</p></div><div class="modal-actions"><button id="update_release" type="button" class="secondary" hidden onclick="openLatestRelease()">${tr("transfers.open_latest_release")}</button><button id="update_commands" type="button" class="secondary" hidden onclick="copyLinuxMigrationCommands()">${tr("transfers.copy_installer_command")}</button><button id="update_install" type="button" data-icon="update" hidden onclick="requestUpdate('install')">${tr("transfers.update_in_background")}</button></div>`, '', null);
  if(latestUpdateInfo)renderUpdate(latestUpdateInfo);
  await requestUpdate('check');
}

return {open:openUpdate,stop:stopUpdatePoll,request:requestUpdate,render:renderUpdate,hint:renderUpdateHint,checkHint:checkUpdateHint,openRelease:openLatestRelease,copyCommands:copyLinuxMigrationCommands,info:()=>latestUpdateInfo};
}
