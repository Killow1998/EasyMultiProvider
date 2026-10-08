// A display preference, like language/theme. Stored quota samples remain unchanged.
function createQuotaDisplay({read, save}) {
  let mode = read('emp.quotaDisplay', 'remaining') === 'used' ? 'used' : 'remaining';
  function value(remaining) {
    if (remaining == null || !Number.isFinite(Number(remaining))) return null;
    const percent = Math.max(0, Math.min(100, Number(remaining)));
    return Math.round((mode === 'used' ? 100 - percent : percent) * 100) / 100;
  }
  return {
    mode:() => mode,
    value,
    label:tr => mode === 'used' ? tr('已用', 'Used') : tr('剩余', 'Remaining'),
    set(next) {
      if (!['remaining', 'used'].includes(next)) return;
      mode = next;
      save('emp.quotaDisplay', mode);
    },
  };
}
