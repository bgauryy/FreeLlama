"""Outcome contracts for bounded open-loop managed admission measurements."""
import copy
import json
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

import run_load
from load_contract import guard_errors, idle_errors, load_plan, new_report, summarize
from test_workload import manifest


def status(holding=False):
    return {'host':{'holding':holding,'memory_pressure':'normal','sample_age_ms':0,'thermal_throttled':None},
        'backends':{'gpu':{'upstream':'http://127.0.0.1:11541','admission':{'active_units':0,'queue_depth':0,'resource_waiters':0}}},
        'raw_proxy':{'active':0,'waiting':0}}


def response(config, case):
    return {'response':{'model':config['model'],'done':True,'message':{'content':case['expected_text']}},
        'route':{'selected_model':config['model']},'admission':{'queue_wait_ms':4,'total_wait_ms':4},
        'execution':{'observation':{'status':'verified','processor':'gpu'},'runtime_options':
            {'num_ctx':config['num_ctx'],'num_predict':config['num_predict'],'temperature':0,'seed':42},
            'upstream':config['direct_endpoint']}}


def mocked_measure(config,endpoint,token,output,bad=False):
    jobs={};calls=[];lock=__import__('threading').Lock()
    def transport(url,method,body,auth,timeout):
        with lock:
            now=time.monotonic()
            if url.endswith('/_load_fixture'):
                payload={'scope':'nonphysical_synthetic_fixture','inference':False,'model':config['model'],
                    'chat_calls':len(jobs),'scenario':config['control_scenario']}
            elif url.endswith('/api/ps'):
                payload={'models':[] if config['control_scenario']=='resource_hold' else [{'name':config['model']}]}
            elif url.endswith('/health'):
                payload={'status':'ok'}
            elif url.endswith('/status'):
                payload=status()
            elif url.endswith('/config'):
                payload={'settings':{'fixture':True}}
            elif url.endswith('/tasks'):
                calls.append(body)
                number=len(jobs);key=f'job-{number}'
                case=next(c for c in config['cases'] if c['prompt']==body['prompt'])
                jobs[key]={'created':now,'case':case,'cancelled':False}
                return {'http_status':202,'payload':{'deferred':True,'job':{'id':key,'status':'queued'}},'http_seconds':.001}
            else:
                key=url.split('/')[-2] if url.endswith('/cancel') else url.split('/')[-1]
                value=jobs[key]
                if url.endswith('/cancel'):value['cancelled']=True
                if value['cancelled']:
                    job={'id':key,'status':'cancelled','error':{'code':'task_cancelled'}}
                elif now-value['created']>=.18:
                    raw=response(config,value['case'])
                    if bad:raw['response']['done']=False
                    job={'id':key,'status':'completed','result':raw}
                else:job={'id':key,'status':'waiting_for_admission'}
                payload={'job':job}
            return {'http_status':200,'payload':payload,'http_seconds':.001}
    with patch.object(run_load,'http',side_effect=transport):
        run_load.measure(config,endpoint,token,output)
    return calls


def hanging_load(config,endpoint,token,output):
    def hang(url,method,body,auth,timeout):
        if url.endswith('/health'):value={'status':'ok'}
        elif url.endswith('/status'):value=status()
        elif url.endswith('/config'):value={'settings':{}}
        elif url.endswith('/_load_fixture'):value={'scope':'nonphysical_synthetic_fixture','inference':False,'model':config['model'],
            'scenario':config['control_scenario'],'chat_calls':0}
        elif url.endswith('/api/ps'):value={'models':[{'name':config['model']}]}
        else:time.sleep(30);value={}
        return {'http_status':200,'payload':value,'http_seconds':.001}
    with patch.object(run_load,'http',side_effect=hang):run_load.measure(config,endpoint,token,output)


class LoadContracts(unittest.TestCase):
    def setUp(self):
        self.directory=tempfile.TemporaryDirectory();self.addCleanup(self.directory.cleanup)
        self.root=Path(self.directory.name)
        (self.root/'workload.json').write_text(json.dumps(manifest()))
        self.settings={'schema_version':1,'mode':'controlled','workload':'workload.json','arrival_rate_rps':20,
            'duration_seconds':.2,'max_requests':4,'max_outstanding':2,'run_budget_seconds':3,
            'task_timeout_seconds':1,'max_wait_seconds':1,'poll_interval_seconds':.01,'drain_seconds':1,
            'request_timeout_seconds':1,'priority_cycle':['interactive','normal','background'],
            'allowed_error_codes':['admission_queue_full','task_deadline_exceeded','task_cancelled'],
            'cancel_indices':[],'cancel_after_seconds':.1,'control_scenario':'capacity'}
        (self.root/'load.json').write_text(json.dumps(self.settings))
        self.config=load_plan(self.root/'load.json');self.config['direct_endpoint']='http://127.0.0.1:11541'

    def receipt(self):
        report=new_report(self.config,'http://127.0.0.1:11542')
        fixture={'scope':'nonphysical_synthetic_fixture','inference':False,'model':self.config['model'],
            'scenario':self.config['control_scenario']}
        residency={'models':[] if self.config['control_scenario']=='resource_hold' else [{'name':self.config['model']}]}
        report['initial']={'status':status(),'fixture':dict(fixture,chat_calls=0),'residency':residency}
        report['final']={'status':status(),'fixture':dict(fixture,chat_calls=4),'residency':residency}
        for item in report['items']:
            item.update(state='completed',submitted_at_seconds=item['scheduled_at_seconds'],accepted_at_seconds=.01,
                terminal_at_seconds=.2,job_id=f'job-{item["index"]}',accepted_receipt={'deferred':True},
                terminal_receipt={'job':{'id':f'job-{item["index"]}','status':'completed','result':response(self.config,
                    self.config['cases'][item['index']%2])}})
        report['elapsed_seconds']=.3
        summarize(report,self.config)
        return report

    def test_missing_quality_done_or_golden_never_qualifies(self):
        for mutate in (lambda r:r.update(done=False),lambda r:r.update(message={'content':'wrong'})):
            report=self.receipt();mutate(report['items'][0]['terminal_receipt']['job']['result']['response'])
            summarize(report,self.config)
            self.assertEqual(report['verdict'],'reject')
            self.assertIsNone(report['performance']['qualified_completed_requests_per_minute'])

    def test_raw_overload_is_retained_as_predeclared_outcome_not_quality_success(self):
        report=self.receipt();row=report['items'][1]
        row.update(state='failed',terminal_receipt={'job':{'id':row['job_id'],'status':'failed','error':{'code':'admission_queue_full','error':'full'}}})
        summarize(report,self.config)
        self.assertEqual(report['verdict'],'accept',report['failures'])
        self.assertEqual(report['counts']['server_rejected'],1)
        self.assertEqual(report['counts']['quality_completed'],3)
        self.assertEqual(report['items'][1]['terminal_receipt']['job']['error']['error'],'full')

    def test_unexpected_refusal_or_missing_permit_release_rejects(self):
        report=self.receipt();report['final']['status']['backends']['gpu']['admission']['active_units']=1
        summarize(report,self.config);self.assertEqual(report['verdict'],'reject')
        report=self.receipt();report['items'][0].update(state='submission_rejected',http_status=500,
            submission_response={'code':'unexpected_server_error'})
        summarize(report,self.config);self.assertEqual(report['verdict'],'reject')

    def test_initial_guard_block_without_final_observation_does_not_invent_a_leak(self):
        report=new_report(self.config,'http://127.0.0.1:11542')
        report['initial']=self.receipt()['initial']
        report['initial']['status']['host']['holding']=True
        report['fatal_failures']=['initial guard: declared capacity control is resource-held']
        summarize(report,self.config)
        self.assertEqual(report['verdict'],'reject')
        self.assertEqual(report['counts']['submitted'],0)
        self.assertTrue(any('final observation unavailable' in error for error in report['failures']))
        self.assertFalse(any('not released' in error or 'not idle' in error for error in report['failures']))

    def test_unavailable_partial_cleanup_proof_differs_from_observed_nonzero(self):
        missing=idle_errors({'status':{'backends':{'gpu':{'admission':{'active_units':0}}},'raw_proxy':{}}})
        self.assertTrue(missing)
        self.assertTrue(all('unavailable' in error for error in missing))
        for key in ('active_units','queue_depth','resource_waiters'):
            snapshot={'status':status()};snapshot['status']['backends']['gpu']['admission'][key]=1
            self.assertTrue(any('not released' in error for error in idle_errors(snapshot)))
        for key in ('active','waiting'):
            snapshot={'status':status()};snapshot['status']['raw_proxy'][key]=1
            self.assertTrue(any('not idle' in error for error in idle_errors(snapshot)))
    def test_client_cap_abort_and_cancel_have_distinct_accounting(self):
        report=self.receipt();report['items'][1]['state']='client_cap_rejected'
        report['items'][2].update(state='cancelled',terminal_receipt={'job':{'id':report['items'][2]['job_id'],'status':'cancelled','error':{'code':'task_cancelled'}}})
        report['items'][3]['state']='not_submitted_abort'
        self.config['abort_after_seconds']=.1
        summarize(report,self.config)
        self.assertEqual(report['counts']['client_cap_rejected'],1)
        self.assertEqual(report['counts']['cancelled'],1)
        self.assertEqual(report['counts']['not_submitted_abort'],1)
        self.assertEqual(report['counts']['quality_completed'],1)
        self.assertEqual(len(report['performance']['priorities']),3)

    def test_incomplete_or_unlabelled_mock_cannot_claim_inference(self):
        report=self.receipt();report['items'][0]['state']='submitted'
        summarize(report,self.config);self.assertEqual(report['verdict'],'reject')
        report=self.receipt();report['initial']['fixture']['scope']='physical_gpu'
        summarize(report,self.config);self.assertEqual(report['verdict'],'reject')
        self.assertEqual(report['measurement_scope'],'controlled_gateway_lifecycle_no_inference')
        self.assertIsNone(report['performance']['inference_goodput_requests_per_minute'])

    def test_resource_hold_requires_zero_upstream_calls_and_cleanup(self):
        self.config['control_scenario']='resource_hold'
        report=self.receipt();report['initial']['status']['host']['holding']=True
        report['final']['fixture']['chat_calls']=0
        for row in report['items']:
            row.update(state='expired',terminal_receipt={'job':{'id':row['job_id'],'status':'expired','error':{'code':'task_deadline_exceeded'}}})
        summarize(report,self.config);self.assertEqual(report['verdict'],'accept',report['failures'])
        report['final']['fixture']['chat_calls']=1
        summarize(report,self.config);self.assertEqual(report['verdict'],'reject')

    def test_controlled_scenario_and_residency_must_match_declared_experiment(self):
        config=dict(self.config,control_scenario='resource_hold')
        snapshot={'status':status(True),'fixture':{'scope':'nonphysical_synthetic_fixture',
            'inference':False,'model':config['model'],'scenario':'capacity','chat_calls':0},
            'residency':{'models':[{'name':config['model']}]}}
        self.assertTrue(guard_errors(snapshot,config,True))
        snapshot['fixture']['scenario']='resource_hold'
        self.assertTrue(guard_errors(snapshot,config,True))
        snapshot['residency']['models']=[]
        self.assertEqual(guard_errors(snapshot,config,True),[])

    def test_actual_mock_http_installed_inventory_and_residency_are_scenario_specific(self):
        import threading
        for scenario in ('resource_hold','capacity'):
            server=run_load.create_mock_upstream(self.config,0,0,scenario)
            thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
            try:
                endpoint=f'http://127.0.0.1:{server.server_port}'
                tags=run_load.http(endpoint+'/api/tags','GET',None,None,1)['payload']['models']
                models=run_load.http(endpoint+'/api/ps','GET',None,None,1)['payload']['models']
                marker=run_load.http(endpoint+'/_load_fixture','GET',None,None,1)['payload']
                self.assertEqual(tags[0]['name'],self.config['model'])
                self.assertEqual(models,[] if scenario=='resource_hold' else tags)
                self.assertEqual(marker['scenario'],scenario)
                self.assertIs(marker['inference'],False)
            finally:
                server.shutdown();server.server_close();thread.join(timeout=1)

    def test_actual_http_cold_control_uses_configured_evidence_before_admission(self):
        import threading
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        submitted=[];config=dict(self.config,control_scenario='resource_hold')
        config['allowed_error_codes']=['resource_admission_unavailable']
        upstream=run_load.create_mock_upstream(config,0,0,'resource_hold')
        class Gateway(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def reply(self,value,code=200):
                data=json.dumps(value).encode();self.send_response(code)
                self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
            def do_GET(self):
                if self.path.endswith('/status'):self.reply(status(True))
                elif '/jobs/' in self.path:self.reply({'job':{'id':self.path.rsplit('/',1)[-1],
                    'status':'failed','error':{'code':'resource_admission_unavailable'}}})
                else:self.reply({})
            def do_POST(self):
                body=json.loads(self.rfile.read(int(self.headers['Content-Length'])));submitted.append(body)
                if body.get('min_placement_evidence')!='configured':
                    self.reply({'code':'placement_evidence_unavailable'},422);return
                self.reply({'deferred':True,'job':{'id':f'job-{len(submitted)}','status':'queued'}},202)
        gateway=ThreadingHTTPServer(('127.0.0.1',0),Gateway)
        threads=[threading.Thread(target=s.serve_forever,daemon=True) for s in (upstream,gateway)]
        for thread in threads:thread.start()
        try:
            config['direct_endpoint']=f'http://127.0.0.1:{upstream.server_port}'
            output=self.root/'cold-http.json'
            run_load.measure(config,f'http://127.0.0.1:{gateway.server_port}',None,output)
            report=run_load.read_receipt(output)
            self.assertEqual(report['verdict'],'accept',report['failures'])
            self.assertEqual(len(submitted),4)
            self.assertTrue(all(body['min_placement_evidence']=='configured' for body in submitted))
            self.assertEqual(report['initial']['residency']['models'],[])
            self.assertEqual(report['final']['fixture']['chat_calls'],0)
            self.assertIsNone(report['performance']['inference_goodput_requests_per_minute'])
        finally:
            for server in (upstream,gateway):server.shutdown();server.server_close()
            for thread in threads:thread.join(timeout=1)

    def test_zero_age_is_fresh_but_invalid_or_stale_age_is_rejected(self):
        snapshot=self.receipt()['initial']
        self.assertEqual(guard_errors(snapshot,self.config,True),[])
        for age in (-1,float('nan'),float('inf'),2001,None,True):
            snapshot['status']['host']['sample_age_ms']=age
            self.assertTrue(guard_errors(snapshot,self.config,True),age)

    def test_resident_body_preserves_observed_physical_evidence_requirement(self):
        case=self.config['cases'][0]
        resident=run_load.load_task_body(dict(self.config,mode='resident'),case,'interactive')
        controlled=run_load.load_task_body(self.config,case,'interactive')
        self.assertEqual(resident['min_placement_evidence'],'observed')
        self.assertEqual(controlled['min_placement_evidence'],'configured')
        report=new_report(self.config,'http://127.0.0.1:11542')
        self.assertIn('never physical GPU proof',report['placement_evidence_boundary']['observation_scope'])

    def test_production_open_loop_keeps_offered_times_and_records_caller_cap_drops(self):
        output=self.root/'openloop.json'
        calls=mocked_measure(self.config,'http://127.0.0.1:11542',None,output)
        report=run_load.read_receipt(output)
        self.assertEqual(report['verdict'],'accept',report['failures'])
        self.assertEqual(len(calls),2)
        self.assertEqual(report['counts']['client_cap_rejected'],2)
        self.assertEqual([r['scheduled_at_seconds'] for r in report['items']],[0,.05,.1,.15])
        self.assertTrue(all(r['terminal_at_seconds']<.18 for r in report['items'] if r['state']=='client_cap_rejected'))

    def test_production_cancel_receipt_is_retained_and_first_bad_quality_aborts(self):
        config=dict(self.config,cancel_indices=[0],cancel_after_seconds=.025)
        output=self.root/'cancel.json';mocked_measure(config,'http://127.0.0.1:11542',None,output)
        report=run_load.read_receipt(output)
        self.assertEqual(report['verdict'],'accept',report['failures'])
        self.assertEqual(report['items'][0]['state'],'cancelled')
        self.assertEqual(report['items'][0]['cancel_receipt']['payload']['job']['status'],'cancelled')
        output=self.root/'bad.json';mocked_measure(self.config,'http://127.0.0.1:11542',None,output,bad=True)
        report=run_load.read_receipt(output)
        self.assertEqual(report['verdict'],'reject')
        self.assertIsNone(report['performance']['qualified_completed_requests_per_minute'])

    def test_external_deadline_retains_submissions_without_waiting_on_hung_transport(self):
        config=dict(self.config,run_budget_seconds=.4)
        started=time.monotonic()
        report=run_load.run_bounded(config,'http://127.0.0.1:11542',None,self.root/'hang.json',worker=hanging_load,
            report_factory=new_report,report_summary=summarize,receipt_reader=run_load.read_receipt)
        self.assertLess(time.monotonic()-started,2)
        self.assertEqual(report['counts']['submitted'],2)
        self.assertEqual(report['counts']['client_cap_rejected'],2)
        self.assertEqual(report['verdict'],'reject')
        self.assertEqual(report['upstream_cancellation'],'unknown')

    def test_terminal_receipt_cannot_claim_completion_for_another_job_or_state(self):
        report=self.receipt();report['items'][0]['terminal_receipt']['job']['id']='another-job'
        summarize(report,self.config);self.assertEqual(report['verdict'],'reject')
        report=self.receipt();report['items'][0]['terminal_receipt']['job']['status']='running'
        summarize(report,self.config);self.assertEqual(report['verdict'],'reject')

    def test_malformed_quality_sections_do_not_destroy_failure_receipt(self):
        report=self.receipt();report['items'][0]['terminal_receipt']['job']['result']['admission']=[]
        summarize(report,self.config)
        self.assertEqual(report['verdict'],'reject')
        self.assertEqual(len(report['items']),4)

    def test_invalid_or_unbounded_plan_fails_before_http(self):
        for update in ({'max_requests':65},{'max_outstanding':0},{'run_budget_seconds':61},{'arrival_rate_rps':float('nan')},
                       {'cancel_indices':[99]},{'unexpected':True}):
            (self.root/'load.json').write_text(json.dumps(dict(self.settings,**update)))
            with self.subTest(update=update),patch.object(run_load,'http') as transport:
                with self.assertRaises(ValueError):load_plan(self.root/'load.json')
                transport.assert_not_called()


if __name__=='__main__':unittest.main()
