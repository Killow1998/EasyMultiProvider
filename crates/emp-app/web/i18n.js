// All product copy is registered before the page creates its controllers.
const uiCopy = Object.create(null);
function registerUiCopy(entries) { Object.assign(uiCopy, entries); }
function translateUiCopy(key, language, values = {}) {
  const text = uiCopy[key]?.[language === 'en' ? 1 : 0] ?? key;
  return text.replace(/\{([A-Za-z][A-Za-z0-9_]*)\}/g, (placeholder, name) =>
    Object.hasOwn(values, name) ? String(values[name]) : placeholder);
}
