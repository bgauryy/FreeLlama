#!/usr/bin/env python3
"""Bounded open-loop deferred admission sensor and explicitly nonphysical mock upstream."""
from __future__ import annotations
import argparse
import copy
import json
import math
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from load_contract import FINISHED, TERMINAL, completed_quality, guard_errors, load_plan, new_report, summarize
from run_workload import reject_nonfinite, run_bounded, task_body
from run_comparison import observe as observe_resident
from comparison_contract import validate_endpoints
from workload_contract import as_object, load_workload, write_report


def http(url,method,body,token,timeout):
    headers={'accept':'application/json'}
    if token:headers['authorization']='Bearer '+token
    if body is not None:headers['content-type']='application/json'
    request=Request(url,data=json.dumps(body).encode() if body is not None else None,headers=headers,method=method)
    started=time.monotonic()
    try:
        response=urlopen(request,timeout=timeout)
    except HTTPError as error:
        response=error
    with response:
        payload=json.loads(response.read(),parse_constant=reject_nonfinite)
        code=response.code
    if not isinstance(payload,dict):raise ValueError('HTTP receipt must be a JSON object')
    return {'http_status':code,'payload':payload,'http_seconds':time.monotonic()-started}


def observe(config,endpoint,token,deadline):
    if config['mode']=='resident':return observe_resident(config,endpoint,token,deadline)
    snapshot={}
    for name,base,path,auth in [('health',endpoint,'/_freellama/v1/health',token),('status',endpoint,'/_freellama/v1/status',token),
        ('runtime',endpoint,'/_freellama/v1/config',token),('fixture',config['direct_endpoint'],'/_load_fixture',None),
        ('residency',config['direct_endpoint'],'/api/ps',None)]:
        remaining=deadline-time.monotonic()
        if remaining<=0:raise TimeoutError('whole-run deadline during observation')
        value=http(base+path,'GET',None,auth,min(config['request_timeout_seconds'],remaining))
        if value['http_status']!=200:raise RuntimeError(f'{name} observation HTTP {value}')
        snapshot[name]=value['payload']
    snapshot['observed_at_unix_seconds']=time.time()
    return snapshot


def apply(report,event):
    kind=event['kind']
    if kind=='item':report['items'][event['index']]=event['value']
    elif kind=='sample':report['status_samples'].append(event['value'])
    elif kind=='fatal':report['fatal_failures'].append(event['value'])
    else:report[kind]=event['value']
    report['journal_applied']=event['sequence']


def read_receipt(path):
    report=json.loads(path.read_text())
    journal=path.with_suffix('.events')
    if journal.exists():
        for line in journal.read_text(errors='replace').splitlines():
            try:event=json.loads(line)
            except json.JSONDecodeError:
                report['fatal_failures'].append('incomplete journal event');break
            if event['sequence']>report['journal_applied']:
                if event['sequence']!=report['journal_applied']+1:
                    report['fatal_failures'].append('journal sequence incomplete');break
                apply(report,event)
    return report


def load_task_body(config,case,priority):
    body=task_body(config,case,config['task_timeout_seconds'])
    body.update(defer=True,priority=priority,max_wait_seconds=config['max_wait_seconds'],
        min_placement_evidence='observed' if config['mode']=='resident' else 'configured')
    return body


def measure(config,endpoint,token,output):
    report=new_report(config,endpoint);write_report(output,report)
    journal=output.with_suffix('.events');journal.write_text('')
    started=time.monotonic();deadline=started+config['run_budget_seconds']
    def emit(kind,**fields):
        event={'sequence':report['journal_applied']+1,'kind':kind,**fields}
        with journal.open('a') as stream:stream.write(json.dumps(event,separators=(',',':'),allow_nan=False)+'\n')
        apply(report,copy.deepcopy(event))
    def update(row):emit('item',index=row['index'],value=row)
    pending={};next_item=0;aborted=False;cleanup=False;next_sample=0
    try:
        initial=observe(config,endpoint,token,deadline);emit('initial',value=initial)
        errors=guard_errors(initial,config,True)
        if errors:raise RuntimeError('initial guard: '+'; '.join(errors))
        # Submission and polling use bounded workers; the arrival clock never waits on job completion.
        with ThreadPoolExecutor(max_workers=min(66,config['max_outstanding']+2)) as executor:
            def submit(kind,row,path,body=None):
                timeout=min(config['request_timeout_seconds'],max(.001,deadline-time.monotonic()))
                future=executor.submit(http,endpoint+path,'POST' if kind in ('submit','cancel') else 'GET',body,token,timeout)
                pending[future]=(kind,row['index'] if row is not None else None)
            while time.monotonic()<deadline:
                now=time.monotonic()-started
                for future in [f for f in pending if f.done()]:
                    kind,index=pending.pop(future)
                    if kind=='sample':
                        try:
                            result=future.result();emit('sample',value={'at_seconds':now,**result})
                            host=as_object(as_object(result.get('payload')).get('host'))
                            if host.get('memory_pressure')!='normal' or host.get('thermal_throttled') is True:
                                raise RuntimeError('host pressure/thermal state changed')
                            if config['mode']=='controlled' and config['control_scenario']=='capacity' and host.get('holding') is not False:
                                raise RuntimeError('capacity control became resource-held; overload isolation unavailable')
                        except Exception as error:
                            emit('fatal',value=f'status sample: {error}');aborted=True
                        continue
                    row=copy.deepcopy(report['items'][index]);row.pop('operation_pending',None)
                    try:result=future.result()
                    except Exception as error:
                        row.setdefault('transport_errors',[]).append({'operation':kind,'type':type(error).__name__,'message':str(error)})
                        if kind=='submit':row.update(state='submission_transport_error',terminal_at_seconds=now);aborted=True
                        else:row['next_poll_at_seconds']=now+config['poll_interval_seconds']
                        update(row);continue
                    if kind=='submit':
                        row.update(http_status=result['http_status'],submission_response=result['payload'],submission_http_seconds=result['http_seconds'])
                        job=as_object(result['payload'].get('job'))
                        if result['http_status']==202 and result['payload'].get('deferred') is True and isinstance(job.get('id'),str):
                            row.update(state='accepted',job_id=job['id'],accepted_at_seconds=now,accepted_receipt=result['payload'])
                        else:row.update(state='submission_rejected',terminal_at_seconds=now)
                    else:
                        job=as_object(result['payload'].get('job'));state=job.get('status')
                        row.setdefault('transitions',[]).append({'status':state,'observed_at_seconds':now,'reason':job.get('reason')})
                        if kind=='cancel':row.update(cancel_receipt=result,cancel_http_seconds=result['http_seconds'])
                        if result['http_status']==200 and job.get('id')==row.get('job_id') and state in TERMINAL:
                            row.update(state=state,terminal_receipt=result['payload'],terminal_at_seconds=now)
                            if state=='completed' and completed_quality(row,config,initial):
                                emit('fatal',value=f'item{index}: completed result failed frozen quality');aborted=True
                        else:
                            row['next_poll_at_seconds']=now+config['poll_interval_seconds']
                            if result['http_status']!=200:row.setdefault('poll_errors',[]).append(result)
                    update(row)
                abort_at=config.get('abort_after_seconds')
                if abort_at is not None and now>=abort_at:aborted=True
                while next_item<len(report['items']) and (aborted or now>=report['items'][next_item]['scheduled_at_seconds']):
                    row=copy.deepcopy(report['items'][next_item]);next_item+=1
                    active=sum(r['state'] not in FINISHED and r['state']!='planned' for r in report['items'])
                    if aborted:row.update(state='not_submitted_abort',terminal_at_seconds=now)
                    elif active>=config['max_outstanding']:row.update(state='client_cap_rejected',terminal_at_seconds=now)
                    else:
                        row.update(state='submitted',submitted_at_seconds=now,operation_pending=True)
                        case=config['cases'][row['index']%len(config['cases'])]
                        body=load_task_body(config,case,row['priority'])
                        row['request']=body;submit('submit',row,'/_freellama/v1/tasks',body)
                    update(row)
                if now>=config['duration_seconds']+config['drain_seconds'] or aborted:cleanup=True
                for original in report['items']:
                    if not original.get('job_id') or original['state'] in FINISHED or original.get('operation_pending'):continue
                    row=copy.deepcopy(original)
                    want_cancel=cleanup or row['index'] in config['cancel_indices'] and now-row['accepted_at_seconds']>=config['cancel_after_seconds']
                    if want_cancel and not row.get('cancel_requested_at_seconds'):
                        row.update(operation_pending=True,cancel_requested_at_seconds=now)
                        submit('cancel',row,f'/_freellama/v1/jobs/{row["job_id"]}/cancel',{})
                    elif now>=row.get('next_poll_at_seconds',0):
                        row['operation_pending']=True;submit('poll',row,f'/_freellama/v1/jobs/{row["job_id"]}')
                    else:continue
                    update(row)
                if now>=next_sample and not any(kind=='sample' for kind,_ in pending.values()):
                    submit('sample',None,'/_freellama/v1/status');next_sample=now+.25
                if next_item==len(report['items']) and all(r['state'] in FINISHED for r in report['items']) and not pending:break
                time.sleep(.005)
        if any(row['state'] not in FINISHED for row in report['items']):emit('fatal',value='whole-run deadline; accepted/submitted ownership remains incomplete')
        emit('final',value=observe(config,endpoint,token,deadline))
    except Exception as error:emit('fatal',value=f'{type(error).__name__}: {error}')
    finally:
        report['elapsed_seconds']=round(time.monotonic()-started,6);summarize(report,config);write_report(output,report)


def create_mock_upstream(config,port,delay,scenario):
    if scenario not in ('resource_hold','capacity'):raise ValueError('explicit mock scenario required')
    lock=threading.Lock();state={'chat_calls':0,'active_handlers':0}
    marker={'scope':'nonphysical_synthetic_fixture','inference':False,'model':config['model'],
        'delay_seconds':delay,'scenario':scenario}
    row={'name':config['model'],'model':config['model'],'digest':'c'*64,'size':1024,'size_vram':1024,
        'context_length':config['num_ctx'],'observation_scope':'nonphysical_synthetic_fixture'}
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def respond(self,value,status=200):
            data=json.dumps(value).encode();self.send_response(status);self.send_header('Content-Type','application/json')
            self.send_header('Content-Length',str(len(data)));self.end_headers()
            try:self.wfile.write(data)
            except (BrokenPipeError,ConnectionResetError):pass
        def do_GET(self):
            if self.path=='/_load_fixture':
                with lock:value={**marker,**state}
                self.respond(value)
            elif self.path=='/api/tags':self.respond({'models':[row]})
            elif self.path=='/api/ps':self.respond({'models':[] if scenario=='resource_hold' else [row]})
            elif self.path=='/api/version':self.respond({'version':'controlled-mock-v1'})
            else:self.respond({'error':'unknown fixture route'},404)
        def do_POST(self):
            body=json.loads(self.rfile.read(int(self.headers.get('Content-Length',0))))
            if self.path=='/api/show':self.respond({'capabilities':['completion','vision'],'model_info':{
                'general.architecture':'llama','llama.context_length':config['num_ctx']}});return
            if self.path!='/api/chat':self.respond({'error':'unsupported fixture operation'},404);return
            with lock:state['chat_calls']+=1;state['active_handlers']+=1
            started=time.monotonic()
            time.sleep(delay)
            prompt=as_object((body.get('messages') or [{}])[-1]).get('content')
            case=next((case for case in config['cases'] if case['prompt']==prompt),None)
            value={'model':config['model'],'done':True,'message':{'role':'assistant','content':case['expected_text'] if case else 'UNDECLARED FIXTURE INPUT'},
                'total_duration':int((time.monotonic()-started)*1e9),'eval_count':1,'prompt_eval_count':1,
                'load_duration':0,'prompt_eval_duration':0,'eval_duration':int(delay*1e9),
                'observation_scope':'nonphysical_synthetic_fixture'}
            self.respond(value)
            with lock:state['active_handlers']-=1
    server=ThreadingHTTPServer(('127.0.0.1',port),Handler)
    server.fixture_marker=marker
    return server


def mock_upstream(workload,port,delay,scenario):
    server=create_mock_upstream(load_workload(workload),port,delay,scenario)
    print(json.dumps({'endpoint':f'http://127.0.0.1:{server.server_port}',**server.fixture_marker}),flush=True)
    try:server.serve_forever()
    finally:server.server_close()


def main():
    parser=argparse.ArgumentParser(description=__doc__);commands=parser.add_subparsers(dest='command',required=True)
    run=commands.add_parser('run');run.add_argument('--plan',type=Path,required=True);run.add_argument('--endpoint',required=True)
    run.add_argument('--upstream',required=True);run.add_argument('--output',type=Path,required=True);run.add_argument('--auth-token-file',type=Path)
    mock=commands.add_parser('mock-upstream');mock.add_argument('--workload',type=Path,required=True);mock.add_argument('--port',type=int,required=True)
    mock.add_argument('--delay-seconds',type=float,default=1)
    mock.add_argument('--scenario',choices=('resource_hold','capacity'),required=True)
    args=parser.parse_args()
    if args.command=='mock-upstream':
        if not 1<=args.port<=65535 or not math.isfinite(args.delay_seconds) or not 0<=args.delay_seconds<=15:parser.error('bounded mock port/delay required')
        mock_upstream(args.workload,args.port,args.delay_seconds,args.scenario);return 0
    try:
        endpoint=args.endpoint.rstrip('/');upstream=args.upstream.rstrip('/');validate_endpoints(endpoint,upstream)
        config=load_plan(args.plan);config['direct_endpoint']=upstream
        token=args.auth_token_file.read_text().strip() if args.auth_token_file else None
    except (ValueError,OSError) as error:parser.error(str(error))
    report=run_bounded(config,endpoint,token,args.output,worker=measure,report_factory=new_report,report_summary=summarize,receipt_reader=read_receipt)
    print(json.dumps({'verdict':report['verdict'],'counts':report['counts'],'failures':report['failures']},indent=2))
    return 0 if report['verdict']=='accept' else 1


if __name__=='__main__':raise SystemExit(main())
