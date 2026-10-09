// Report windows share one request lifecycle. Changing filters cancels the old
// request; only the newest result may update the window. Errors keep its data.
function createReportQuery({api, onState, onResult}) {
  let revision = 0, controller = null;
  async function run(path, options = {}) {
    const selected = ++revision;
    controller?.abort();
    controller = new AbortController();
    onState(true, '');
    let error = '';
    try {
      const result = await api(path, {...options, signal:controller.signal});
      if (selected === revision) onResult(result);
    } catch (failure) {
      if (selected === revision && failure.name !== 'AbortError') error = failure.message;
    } finally {
      if (selected === revision) { controller = null; onState(false, error); }
    }
  }
  function cancel() { revision++; controller?.abort(); controller = null; onState(false, ''); }
  return {run, cancel};
}

function setReportState(root, loading, error, tr) {
  root.setAttribute('aria-busy', String(loading));
  const status = root.querySelector('[data-report-status]');
  status.textContent = loading ? tr('正在查询…','Loading…') : error;
  status.dataset.loading = String(loading);
  for (const button of root.querySelectorAll('[data-report-action]')) {
    button.disabled = loading;
    button.classList.toggle('report-loading', loading && button.dataset.reportAction === 'refresh');
  }
}
