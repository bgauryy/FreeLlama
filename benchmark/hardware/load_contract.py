"""Predeclared arrival schedule, quality and lifecycle receipt contracts."""
from __future__ import annotations
import math
from collections import Counter
from pathlib import Path
import json
from workload_contract import as_object, distribution, load_workload, positive_number, quality_errors
from comparison_contract import chat_options, profile_errors, profile_identity

TERMINAL = {'completed','failed','cancelled','expired'}
FINISHED = TERMINAL | {'client_cap_rejected','not_submitted_abort','submission_rejected','submission_transport_error'}
PRIORITIES = ('interactive','normal','background')


def load_plan(path: Path) -> dict:
    settings=json.loads(path.read_text())
    allowed={'schema_version','mode','workload','arrival_rate_rps','duration_seconds','max_requests','max_outstanding',
        'run_budget_seconds','task_timeout_seconds','max_wait_seconds','request_timeout_seconds','poll_interval_seconds',
        'drain_seconds','priority_cycle','allowed_error_codes','cancel_indices','cancel_after_seconds','abort_after_seconds','control_scenario'}
    if not isinstance(settings,dict) or set(settings)-allowed:raise ValueError('unknown load plan fields')
    if type(settings.get('schema_version')) is not int or settings['schema_version']!=1:raise ValueError('schema_version must be1')
    if settings.get('mode') not in ('resident','controlled'):raise ValueError('mode must be resident or controlled')
    if not isinstance(settings.get('workload'),str):raise ValueError('workload file required')
    config=load_workload(path.parent/settings['workload'])
    limits={'arrival_rate_rps':(.1,200,20),'duration_seconds':(.05,40,3),'max_requests':(1,64,60),
        'max_outstanding':(1,64,16),'run_budget_seconds':(.1,60,20),'task_timeout_seconds':(1,30,2),
        'max_wait_seconds':(1,10,1),'request_timeout_seconds':(.05,5,2),'poll_interval_seconds':(.005,1,.05),
        'drain_seconds':(.1,15,5),'cancel_after_seconds':(.01,30,.2)}
    integers={'max_requests','max_outstanding','task_timeout_seconds','max_wait_seconds'}
    for key,(minimum,maximum,default) in limits.items():
        value=settings.get(key,default)
        if not positive_number(value) or not minimum<=value<=maximum or key in integers and type(value) is not int:
            raise ValueError(f'{key} outside bounded range [{minimum},{maximum}]')
        settings[key]=value
    if settings['duration_seconds']+settings['drain_seconds']>=settings['run_budget_seconds']:
        raise ValueError('whole budget must leave time after arrivals and drain for cleanup')
    priorities=settings.setdefault('priority_cycle',list(PRIORITIES))
    if not isinstance(priorities,list) or not 1<=len(priorities)<=16 or any(p not in PRIORITIES for p in priorities):
        raise ValueError('invalid priority_cycle')
    errors=settings.setdefault('allowed_error_codes',[])
    if not isinstance(errors,list) or len(errors)>16 or any(not isinstance(e,str) or not e for e in errors):
        raise ValueError('allowed_error_codes must be predeclared strings')
    if settings['mode']=='controlled' and settings.get('control_scenario') not in ('capacity','resource_hold'):
        raise ValueError('controlled scenario must be capacity or resource_hold')
    config.update(settings)
    config['planned_requests']=min(config['max_requests'],math.ceil(config['arrival_rate_rps']*config['duration_seconds']-1e-9))
    cancels=config.setdefault('cancel_indices',[])
    if not isinstance(cancels,list) or len(set(cancels))!=len(cancels) or any(type(i) is not int or not 0<=i<config['planned_requests'] for i in cancels):
        raise ValueError('cancel_indices must name distinct planned request indices')
    abort=config.get('abort_after_seconds')
    if abort is not None and (not positive_number(abort) or not 0<abort<config['duration_seconds']):
        raise ValueError('abort_after_seconds must fall inside arrival duration')
    return config


def new_report(config:dict,endpoint:str)->dict:
    return {'schema_version':1,'measurement_scope':('resident_model_open_loop' if config['mode']=='resident' else
        'controlled_gateway_lifecycle_no_inference'),'endpoint':endpoint,'mode':config['mode'],
        'plan':{k:v for k,v in config.items() if k!='cases'},'initial':None,'final':None,
        'items':[{'index':i,'case_id':config['cases'][i%len(config['cases'])]['id'],
            'priority':config['priority_cycle'][i%len(config['priority_cycle'])],
            'scheduled_at_seconds':i/config['arrival_rate_rps'],'state':'planned','transitions':[]}
            for i in range(config['planned_requests'])], 'status_samples':[], 'fatal_failures':[], 'failures':[],
        'verdict':'reject','journal_applied':0,'upstream_cancellation':'unknown',
        'placement_evidence_boundary':{'requested_minimum':'observed' if config['mode']=='resident' else 'configured',
            'observation_scope':'physical resident model/process profile' if config['mode']=='resident' else
                'nonphysical synthetic fixture; configured assignment and fake PS are never physical GPU proof'},
        'limits':{'maximum_requests':64,'maximum_wall_seconds':60},
        'timing_scope':'open-loop fixed offered times; observed terminal latency includes polling; no hidden caller-cap throttling'}


def idle_errors(snapshot:dict)->list:
    status=as_object(snapshot.get('status'))
    if not status:return ['status observation unavailable; local permit release and raw idleness unqualified']
    admission=as_object(as_object(as_object(status.get('backends')).get('gpu')).get('admission'))
    errors=[]
    admission_keys=('active_units','queue_depth','resource_waiters')
    if any(not positive_number(admission.get(key)) for key in admission_keys):
        errors.append('local admission observation unavailable or malformed; permit release unqualified')
    if any(positive_number(admission.get(key)) and admission[key]>0 for key in admission_keys):
        errors.append('observed local permits/queue/resource waiters not released')
    raw=as_object(status.get('raw_proxy'))
    if any(not positive_number(raw.get(key)) for key in ('active','waiting')):
        errors.append('raw proxy observation unavailable or malformed; idleness unqualified')
    if any(positive_number(raw.get(key)) and raw[key]>0 for key in ('active','waiting')):
        errors.append('observed raw proxy not idle')
    return errors


def guard_errors(snapshot:dict,config:dict,initial:bool)->list:
    if not snapshot:return [f'{"initial" if initial else "final"} observation unavailable; qualification incomplete']
    if config['mode']=='resident':return profile_errors(snapshot,config)
    errors=idle_errors(snapshot)
    fixture=as_object(snapshot.get('fixture'))
    if (fixture.get('scope')!='nonphysical_synthetic_fixture' or fixture.get('inference') is not False or
            fixture.get('model')!=config['model']):errors.append('controlled fixture label/model missing; physical inference not qualified')
    if fixture.get('scenario')!=config['control_scenario']:errors.append('controlled fixture scenario differs from declared experiment')
    models=as_object(snapshot.get('residency')).get('models')
    if config['control_scenario']=='resource_hold':
        if models!=[]:errors.append('resource-hold control must have no resident runner')
    elif not isinstance(models,list) or len(models)!=1 or as_object(models[0]).get('name')!=config['model']:
        errors.append('capacity control must have exactly its synthetic resident runner')
    host=as_object(as_object(snapshot.get('status')).get('host'))
    if host.get('memory_pressure')!='normal' or host.get('thermal_throttled') is True:errors.append('host pressure/thermal guard not ready')
    age=host.get('sample_age_ms')
    if not positive_number(age) or not 0<=age<=2000:errors.append('host observation stale')
    if initial and host.get('holding') is not (config['control_scenario']=='resource_hold'):
        errors.append('declared controlled resource state differs from observed host hold')
    if initial and fixture.get('chat_calls')!=0:errors.append('controlled upstream was not initially unused')
    return errors


def completed_quality(item:dict,config:dict,initial:dict)->list:
    job=as_object(as_object(item.get('terminal_receipt')).get('job'))
    payload=as_object(job.get('result'))
    case=next((c for c in config['cases'] if c['id']==item['case_id']),{})
    errors=quality_errors({'status':'success','payload':payload},case,config['model'])
    if as_object(payload.get('response')).get('model')!=config['model']:errors.append('raw completion exact model missing')
    execution=as_object(payload.get('execution'))
    if execution.get('runtime_options')!=chat_options(config) or execution.get('upstream')!=config['direct_endpoint']:
        errors.append('applied controls/upstream differ from frozen workload')
    if config['mode']=='resident':
        from comparison_contract import resident
        digest=resident(initial,config['model']).get('digest');observation=as_object(execution.get('observation'))
        if execution.get('model_digest')!=digest or observation.get('digest')!=digest or observation.get('context_length')!=config['num_ctx']:
            errors.append('physical revision/context observation mismatched')
    return errors


def summarize(report:dict,config:dict)->None:
    initial,final=as_object(report.get('initial')),as_object(report.get('final'))
    failures=list(report['fatal_failures'])+guard_errors(initial,config,True)+guard_errors(final,config,False)
    if config['mode']=='resident' and profile_identity(initial,config)!=profile_identity(final,config):failures.append('resident process/model/runtime profile changed')
    items=report['items'];counts=Counter(row.get('state') for row in items);quality=0
    if len(items)!=config['planned_requests'] or [r.get('index') for r in items]!=list(range(config['planned_requests'])):
        failures.append('planned request set incomplete or duplicated')
    for row in items:
        state=row.get('state')
        if state in TERMINAL:
            terminal=as_object(as_object(row.get('terminal_receipt')).get('job'))
            if terminal.get('id')!=row.get('job_id') or not row.get('job_id') or terminal.get('status')!=state:
                failures.append(f'item{row["index"]}: terminal receipt does not match owned job/state')
        if state not in FINISHED:failures.append(f'item{row.get("index")}: incomplete {state}')
        elif state=='completed':
            errors=completed_quality(row,config,initial);quality+=not errors
            failures.extend(f'item{row["index"]}: {error}' for error in errors)
        elif state in ('failed','expired','cancelled','submission_rejected'):
            raw=as_object(row.get('submission_response')) if state=='submission_rejected' else as_object(as_object(as_object(row.get('terminal_receipt')).get('job')).get('error'))
            if raw.get('code') not in config['allowed_error_codes']:failures.append(f'item{row["index"]}: undeclared outcome {raw}')
        elif state=='submission_transport_error':failures.append(f'item{row["index"]}: submission ownership unknown after transport failure')
        elif state=='not_submitted_abort' and config.get('abort_after_seconds') is None and not report['fatal_failures']:
            failures.append('undeclared abort')
    jobs=[r.get('job_id') for r in items if r.get('job_id')]
    if len(set(jobs))!=len(jobs):failures.append('duplicate accepted job handles')
    if config['mode']=='controlled':
        calls=as_object(final.get('fixture')).get('chat_calls')
        if config['control_scenario']=='resource_hold' and calls!=0:failures.append('resource-hold control unexpectedly reached upstream')
        if config['control_scenario']=='capacity' and quality<1:failures.append('capacity control has no exact completed response')
    by_priority={}
    for priority in PRIORITIES:
        rows=[r for r in items if r['priority']==priority]
        waits=[as_object(as_object(as_object(as_object(r.get('terminal_receipt')).get('job')).get('result')).get('admission')).get('queue_wait_ms') for r in rows if r.get('state')=='completed']
        latency=[(r['terminal_at_seconds']-r['scheduled_at_seconds'])*1000 for r in rows if positive_number(r.get('terminal_at_seconds'))]
        by_priority[priority]={'offered':len(rows),'outcomes':dict(Counter(r['state'] for r in rows)),
            'accepted':sum(bool(r.get('job_id')) for r in rows),'queue_wait_ms':distribution([v for v in waits if positive_number(v)]),
            'observed_scheduled_to_terminal_ms':distribution(latency)}
    elapsed=report.get('elapsed_seconds',0)
    report['counts']={**dict(counts),'offered':len(items),'submitted':sum('submitted_at_seconds'in r for r in items),
        'accepted':len(jobs),'server_rejected':counts['failed']+counts['submission_rejected'],'quality_completed':quality,
        'requested_cancellations':sum('cancel_requested_at_seconds'in r for r in items)}
    report['performance']={'arrival_rate_rps':config['arrival_rate_rps'],'max_outstanding':config['max_outstanding'],
        'actual_submission_lag_ms':distribution([(r['submitted_at_seconds']-r['scheduled_at_seconds'])*1000 for r in items if 'submitted_at_seconds'in r]),
        'priorities':by_priority,'fairness_scope':'descriptive per-priority progress and wait samples; no statistical weighted fairness claim',
        'qualified_completed_requests_per_minute':60*quality/elapsed if not failures and elapsed>0 else None,
        'inference_goodput_requests_per_minute':60*quality/elapsed if not failures and elapsed>0 and config['mode']=='resident' else None,
        'percentiles':'nearest rank p95 with sample counts'}
    report['completion']={'planned_requests':len(items),'submitted_requests':report['counts']['submitted'],
        'completed_requests':sum(r['state']in FINISHED for r in items),'successful_requests':quality,
        'complete':all(r['state']in FINISHED for r in items)}
    report.update(failures=failures,verdict='accept' if not failures else 'reject')
