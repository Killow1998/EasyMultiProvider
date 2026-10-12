"""Selection contracts: only selected services/settings leave the preview."""
import copy
import unittest
from migration_selection import select_payload, scrub, combine_payloads, PreviewError

class SelectionTests(unittest.TestCase):
    def fixture(self):
        account={'id':'a','prefix':'a','name':'Account','auth_file':'/private/auth','model_context_windows':{'gpt-test':1000}}
        return {'config':{'accounts':[account], 'providers':[{'id':'p','name':'API','api_key_file':'/private/key'},{'id':'q'}],
          'models':[{'id':'p/m','provider':'p','upstream_id':'m'},{'id':'q/m','provider':'q','upstream_id':'m'}],
          'catalog_presentations':{'a/gpt-test':{'catalog_alias':'Old'},'q/m':{'catalog_alias':'Unselected'}},
          'catalog_family_presentations':{'gpt-test':{'catalog_alias':'Old'},'unselected':{'catalog_alias':'Other'}}},
          'accounts':[{'metadata':account,'auth':{'tokens':{'access_token':'fixture'}}}],
          'provider_keys':{'p':'selected-key','q':'other-key'}}

    def test_projection_preserves_source_and_omits_unselected_credentials(self):
        source=self.fixture();before=copy.deepcopy(source)
        result=select_payload(source,[{'id':'p','kind':'provider','name':'Edited','models':[{'id':'p/m','name':'Model','selected':True,'context_window':2048}]}])
        self.assertEqual(source,before)
        self.assertEqual(result['provider_keys'],{'p':'selected-key'})
        self.assertEqual(result['accounts'],[])
        self.assertEqual(result['config']['catalog_presentations'],{})
        self.assertNotIn('api_key_file',result['config']['providers'][0])
        self.assertEqual(result['config']['models'][0]['context_window'],2048)

    def test_subscription_options_and_redacted_preview(self):
        result=select_payload(self.fixture(),[{'id':'a','kind':'account','name':'Renamed','models':[{'id':'gpt-test','name':'Alias','original_name':'GPT','context_window':2000,'selected':False}]}],context=False,rename=False)
        metadata=result['accounts'][0]['metadata']
        self.assertNotIn('model_context_windows',metadata)
        self.assertNotIn('auth_file',metadata)
        self.assertEqual(metadata['hidden_models'],['gpt-test'])
        self.assertNotIn('catalog_alias',result['config']['catalog_presentations']['a/gpt-test'])
        self.assertEqual(result['model_previews']['a'][0]['name'],'GPT')
        self.assertNotIn('context_window',result['model_previews']['a'][0])
        clean=scrub(result)
        self.assertNotIn('auth',clean['accounts'][0]);self.assertNotIn('provider_keys',clean)

    def test_repeated_projection_retains_previous_credentials_without_duplicates(self):
        source=self.fixture();before=copy.deepcopy(source)
        provider=select_payload(source,[{'id':'p','kind':'provider','models':[]}])
        account=select_payload(source,[{'id':'a','kind':'account','models':[]}])
        result=combine_payloads([provider,account,account])
        self.assertEqual(result['provider_keys'],{'p':'selected-key'})
        self.assertEqual(len(result['accounts']),1)
        self.assertEqual(source,before)
        self.assertEqual(len(result['config']['providers']),1)

    def test_native_random_id_remaps_account_models_and_preserves_other_ids(self):
        source=self.fixture()
        source['accounts'][0]['metadata'].update(id='native-login',prefix='native-login',name='Native login')
        source['config']['accounts']= [source['accounts'][0]['metadata']]
        source['config']['catalog_presentations']={'gpt-test':{'catalog_alias':'Chosen'}}
        choice={'id':'@native','kind':'account','export_id':'a9z2','models':[{'id':'gpt-test','selected':True,'name':'Chosen'}]}
        result=select_payload(source,[choice,{'id':'p','kind':'provider','models':[]}])
        meta=result['accounts'][0]['metadata']
        self.assertEqual((meta['id'],meta['prefix'],meta['name']),('a9z2','a9z2','Native login'))
        self.assertIn('a9z2/gpt-test',result['config']['catalog_presentations'])
        self.assertNotIn('gpt-test',result['config']['catalog_presentations'])
        self.assertIn('a9z2',result['model_previews'])
        self.assertEqual(result['config']['providers'][0]['id'],'p')
        with self.assertRaises(PreviewError):select_payload(source,[{**choice,'export_id':'native'}])

if __name__=='__main__':unittest.main()
