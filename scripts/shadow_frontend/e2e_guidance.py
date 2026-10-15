"""Visible service actions, guidance and saved settings against a disposable EMP."""
import argparse
import base64
import contextlib
import json
from pathlib import Path
import tempfile

from candidate_web import candidate_frontend
from e2e_environment import request, start_backend, wait_for
from e2e_refinements import Browser


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--emp', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    checks, layouts = [], []
    web = Path(__file__).resolve().parents[2] / 'crates/emp-app/web'
    with tempfile.TemporaryDirectory(prefix='emp-guidance-') as temp, contextlib.ExitStack() as stack:
        root = Path(temp)
        catalog = root / 'models.json'
        catalog.write_text(json.dumps({'models': [{'slug': 'gpt-fixture', 'display_name': 'Fixture model',
            'context_window': 272000, 'max_context_window': 872000, 'visibility': 'list', 'base_instructions': 'fixture'}]}))
        base, url, _, headers, upstream = start_backend(stack, args.emp.resolve(), root,
            {'native_catalog_path': str(catalog)})
        frontend, _ = candidate_frontend(stack, base, web)
        browser = Browser(stack, root, system_access=True)
        browser.navigate(url.replace(base, frontend))
        browser.execute('managementEvents.stop();clearInterval(quotaStateTimer);quotaStateTimer=null;')
        headers.update({'Origin': base, 'X-EMP-Session': json.loads((root / 'state/web-session.json').read_text())['token']})
        for language in ['zh-CN', 'en']:
            for theme in ['light', 'dark']:
                for width in [390, 800, 1440]:
                    request(browser.base + '/moz/context', {'context': 'chrome'})
                    request(browser.base + '/execute/sync', {'script': 'gBrowser.selectedBrowser.browsingContext.fullZoom=1;', 'args': []})
                    request(browser.base + '/moz/context', {'context': 'content'})
                    request(browser.base + '/window/rect', {'width': width, 'height': 1000})
                    actual_width = browser.execute('return innerWidth;')
                    if actual_width > width:
                        request(browser.base + '/moz/context', {'context': 'chrome'})
                        request(browser.base + '/execute/sync', {'script': 'gBrowser.selectedBrowser.browsingContext.fullZoom=arguments[0];', 'args': [actual_width / width]})
                        request(browser.base + '/moz/context', {'context': 'content'})
                    browser.execute('closeModal();setLanguage(arguments[0]);setTheme(arguments[1]);', language, theme)
                    layout = browser.execute('''
                      const list=document.getElementById('services'), cards=[...list.querySelectorAll('.service-card')];
                      const inside=(a,b)=>a.left>=b.left-1&&a.right<=b.right+1&&a.top>=b.top-1&&a.bottom<=b.bottom+1;
                      return {viewport:innerWidth, pageFits:document.documentElement.scrollWidth<=innerWidth,
                        listFits:list.scrollWidth<=list.clientWidth+1,
                        actionsVisible:cards.every(card=>[...card.querySelectorAll('.service-actions button')].every(button=>
                          parseFloat(getComputedStyle(button).fontSize)>=12&&inside(button.getBoundingClientRect(),card.getBoundingClientRect()))),
                        groupsSeparate:cards.every(card=>{const a=card.querySelector('.service-main').getBoundingClientRect(),b=card.querySelector('.service-actions').getBoundingClientRect();return a.right<=b.left+1||a.bottom<=b.top+1||b.bottom<=a.top+1;})};
                    ''')
                    layouts.append({'language': language, 'theme': theme, **layout})
                    if theme == 'light' or width == 1440:
                        _, png = request(browser.base + '/screenshot')
                        args.output.with_name(f'guidance-{language}-{theme}-{width}.png').write_bytes(base64.b64decode(png['value']))
                    assert all(layout[key] for key in ['pageFits', 'listFits', 'actionsVisible', 'groupsSeparate']), layouts[-1]
                    browser.execute('document.querySelector("[data-ui-action=provider-edit]").click();')
                    assert browser.execute('return document.getElementById("modal_provider_name") && document.querySelector("[role=tab][aria-selected=true]").dataset.uiAction==="provider-edit";')
                    browser.execute('closeModal();document.querySelector("[data-ui-action=provider-model-settings]").click();')
                    wait_for(lambda: browser.execute('return !!document.querySelector("[data-manager-action=toggle]");'))
                    assert browser.execute('const modal=document.querySelector(".modal");return modal.scrollWidth<=modal.clientWidth+1;')
                    browser.execute('closeModal();document.querySelector("[data-ui-action=account-edit]").click();')
                    assert browser.execute('return document.querySelector("[role=tab][aria-selected=true]").dataset.uiAction==="account-edit";')
                    browser.execute('closeModal();')
        checks.append('12 language/theme/viewport combinations: visible action labels, contained controls, direct connection/model entrypoints')

        # Keyboard activation exercises the native disclosure without a custom help controller.
        browser.execute('document.querySelector(".lifecycle-help summary").focus();')
        request(browser.base + '/actions', {'actions': [{'type': 'key', 'id': 'keyboard', 'actions': [
            {'type': 'keyDown', 'value': '\ue007'}, {'type': 'keyUp', 'value': '\ue007'}]}]})
        assert browser.execute('return document.querySelector(".lifecycle-help").open && document.querySelector("[data-i18n=restore_help]").textContent.includes("keeping EMP open");')
        browser.execute('document.querySelector("[data-i18n=add_provider]").click();')
        assert browser.execute('return document.querySelector("[data-ui-action=claude-cpa-choose]").closest(".service-choice-group").querySelector("h3").textContent==="External API service";')
        browser.execute('document.querySelector("[data-ui-action=service-custom-api]").click();document.getElementById("modal_back").click();')
        assert browser.execute('return !!document.querySelector("[data-ui-action=service-custom-api]");')
        checks.append('Keyboard lifecycle help; CPA remains external API; Back returns to service chooser')

        browser.execute('closeModal();document.querySelector("[data-ui-action=provider-edit]").click();')
        before = (root / 'config.json').read_bytes()
        browser.execute('document.getElementById("modal_provider_name").value="Cancelled";closeModal();')
        assert (root / 'config.json').read_bytes() == before
        browser.execute('document.querySelector("[data-ui-action=provider-edit]").click();document.getElementById("modal_provider_name").value="Renamed service";document.getElementById("modal_submit").click();')
        wait_for(lambda: request(base + '/api/config', headers=headers)[1]['providers'][0]['name'] == 'Renamed service')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        browser.execute('document.querySelector("[data-ui-action=provider-model-settings]").click();')
        wait_for(lambda: browser.execute('return !!document.querySelector("[data-manager-action=toggle]");'))
        browser.execute('document.querySelector("[data-manager-action=toggle]").click();')
        wait_for(lambda: request(base + '/api/config', headers=headers)[1]['models'][0]['enabled'] is False)
        wait_for(lambda: browser.execute('return document.querySelector("[data-manager-action=toggle]").getAttribute("aria-pressed")==="false";'))
        browser.execute('document.querySelector("[data-manager-action=toggle]").click();')
        wait_for(lambda: request(base + '/api/config', headers=headers)[1]['models'][0]['enabled'] is True)
        browser.execute('const field=document.getElementById("shadow_model_search");field.value="no-match";field.dispatchEvent(new Event("input"));')
        assert browser.execute('return document.getElementById("shadow_managed_models").textContent.includes("clear your search");')
        browser.execute('closeModal();document.querySelector("[data-ui-action=catalog-display-editor]").click();document.getElementById("modal_catalog_alias").value="My model";document.getElementById("modal_submit").click();')
        wait_for(lambda: browser.execute('return document.getElementById("modal_backdrop").classList.contains("hidden");'))
        assert any(value.get('catalog_alias') == 'My model' for value in request(base + '/api/config', headers=headers)[1]['catalog_family_presentations'].values())
        assert upstream.calls == 0
        checks.append('Cancel preserves config; connection name, visibility and model rename save through real disposable backend; zero inference requests')
        browser.execute('state.catalog_families=[];renderCatalogDisplay();')
        assert browser.execute('return document.getElementById("catalog_display_models").textContent.includes("Add a service");')
        checks.append('Empty catalog and unmatched search explain different next steps')
    args.output.write_text(json.dumps({'passed': True, 'checks': checks, 'layouts': layouts, 'real_model_requests': 0}, indent=2) + '\n')
    print(json.dumps({'passed': True, 'checks': checks}))


if __name__ == '__main__':
    main()
