"""Browser regression for queued details events after bill rows are replaced."""
from e2e_environment import wait_for


def check_bill_lifecycle(browser):
    browser.execute('''
      const root=document.createElement('section');document.body.append(root);
      const totals={requests:1,reported_requests:1,priced_requests:1,unpriced_requests:0,
        input_reports:1,output_reports:1,cache_reports:1,cache_write_reports:1,
        input_tokens:10,output_tokens:5,cached_input_tokens:0,cache_write_tokens:0,cost_nanos:10};
      const page={total:1,offset:0,limit:20,totals,history:{},records:[{...totals,
        session_id:'lifecycle-session',session_name:'Lifecycle',last_at:1}]};
      const probe={root,details:[]};
      probe.ui=createSessionBills({api:(path,options)=>{
        if(!path.includes('series=true'))return Promise.resolve(page);
        probe.details.push({signal:options.signal});
        return Promise.resolve({groups:[{...totals,model:'lifecycle-model'}],totals});
      },tr:key=>key,esc:String,number:String,getLanguage:()=> 'en'});
      window.billLifecycle=probe;
    ''')
    try:
        for action in ['refresh', 'remount']:
            browser.execute('''
              const p=billLifecycle;p.ui.stop();p.details=[];p.ui.mount(p.root,{});
            ''')
            wait_for(lambda: browser.execute('return !!billLifecycle.root.querySelector("[data-session-bill]");'))
            browser.execute('''
              const p=billLifecycle;p.old=p.root.querySelector('[data-session-bill]');
              // Native toggle is queued; the resolved list replaces its row first.
              p.old.open=true;
              if(arguments[0]==='refresh')p.ui.mount(p.root,{},true);
              else {
                p.ui.stop();p.ui.mount(p.root,{}).then(()=>{
                  p.root.querySelector('[data-session-bill]').open=true;
                });
              }
            ''', action)
            wait_for(lambda: browser.execute('''
              const p=billLifecycle,node=p.root.querySelector('[data-session-bill]');
              return node!==p.old&&node.open&&node.querySelector('[data-bill-detail]').textContent.includes('lifecycle-model');
            '''), seconds=5)
            result=browser.execute('''
              const p=billLifecycle;return {requests:p.details.length,
                stale:p.old.querySelector('[data-bill-detail]').textContent,
                aborted:p.details.some(item=>item.signal.aborted)};
            ''')
            assert result=={'requests':1,'stale':'','aborted':False},(action,result)
    finally:
        browser.execute('billLifecycle.ui.stop();billLifecycle.root.remove();delete window.billLifecycle;')
