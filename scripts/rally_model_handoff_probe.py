#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Tyrone Ross, Jr <46267523+tyroneross@users.noreply.github.com>
# SPDX-License-Identifier: Apache-2.0
"""20 synthetic live-model handoffs, batched into four provider invocations.

Provider defaults are preserved. This tests capsule fidelity and target-authored
ACKs via the CLI; it does not test interactive model TUIs or model generality.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import tomllib
import shutil
import shlex


def provider(host, prompt, cwd, env, output, allow_tools=False):
    if host == 'claude':
        cmd = ['claude', '-p', '--restricted', '--strict-mcp-config', '--no-session-persistence', '--output-format', 'json',
               '--tools', 'Bash' if allow_tools else '']
        if allow_tools:
            cmd += ['--allowedTools', f'Bash(python3 {cwd / "receiver_ack.py"} *)', '--permission-mode', 'dontAsk']
    else:
        cmd = [env.get('RALLY_PROBE_CODEX_BINARY','codex'), 'exec', '--ephemeral', '--skip-git-repo-check', '--sandbox',
               'workspace-write' if allow_tools else 'read-only', '--json', '--output-last-message', str(output), '-']
    if host == 'codex':
        # Isolate the acceptance probe from unrelated user MCP services. No
        # configured values or credentials are printed or modified.
        config = Path(os.environ.get('CODEX_HOME', str(Path.home()/'.codex'))) / 'config.toml'
        if config.exists():
            for name in tomllib.loads(config.read_text()).get('mcp_servers', {}):
                cmd[2:2] = ['-c', f'mcp_servers.{name}.enabled=false']
    started = time.monotonic()
    p = subprocess.run(cmd, input=prompt, cwd=cwd, env=env, capture_output=True, text=True, timeout=240)
    output.with_suffix('.trace.jsonl').write_text(p.stdout)
    if p.returncode:
        output.with_suffix('.failure.json').write_text(json.dumps({'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr}))
        raise RuntimeError(f'{host} exited {p.returncode}; diagnostics at {output.with_suffix(".failure.json")}')
    usage = None
    if host == 'claude':
        body = json.loads(p.stdout)
        if body.get('is_error'):
            raise RuntimeError(str(body.get('result'))[:700])
        result, usage = body['result'], body.get('usage')
        output.write_text(result)
    else:
        result = output.read_text()
        for line in p.stdout.splitlines():
            try:
                event = json.loads(line)
                if event.get('type') == 'turn.completed':
                    usage = event.get('usage')
            except ValueError:
                pass
    cleaned = result.strip()
    if cleaned.startswith('```'):
        cleaned = cleaned.split('\n', 1)[1].rsplit('```', 1)[0]
    return json.loads(cleaned), {'host':host, 'seconds':round(time.monotonic()-started, 3), 'usage':usage}


def run(binary, output, codex_binary):
    evidence = {'schema':'agent-rally.optimization.live-model-probe.v1', 'tasks':[], 'calls':[],
                'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),
                'scope':'20 synthetic data tasks in 4 model calls; CLI delivery and receiver-authored ACK, not interactive TUI injection'}
    with tempfile.TemporaryDirectory(prefix='rally-live-model-') as tmp, __import__('contextlib').ExitStack() as cleanup:
        root = Path(tmp)
        (root / '.git').mkdir()
        bindir = root/'bin'
        bindir.mkdir()
        tmux = bindir/'tmux'
        tmux.write_text('#!/bin/sh\nexec '+shlex.quote(shutil.which('tmux'))+' -S '+shlex.quote(str(root/'tmux.sock'))+' "$@"\n')
        tmux.chmod(0o700)
        cleanup.callback(lambda: subprocess.run([str(tmux),'kill-server'],capture_output=True,timeout=10))
        for sender, receiver in [('claude','codex'),('codex','claude')]:
            tag = sender + '-to-' + receiver
            sender_tool, receiver_tool = 'pilot:'+sender+'-sender', 'pilot:'+receiver+'-receiver'
            sender_session, receiver_session = tag+'-sender-session', tag+'-receiver-session'
            base_env = {**os.environ,'RALLY_PROBE_CODEX_BINARY':str(codex_binary),'RALLY_HOOKS':'off','RALLY_DAEMON_AUTOSTART':'0','PATH':str(bindir)+os.pathsep+os.environ.get('PATH','')}
            def rally(tool, session, args):
                p = subprocess.run([str(binary),*args,'--tool',tool,'--json'],cwd=root,
                                   env={**base_env,'RALLY_SESSION_ID':session},capture_output=True,text=True,timeout=30)
                if p.returncode:
                    raise RuntimeError(p.stderr[-800:])
                body=json.loads(p.stdout)
                if not body.get('ok') or body.get('command')=='watchdog':
                    raise RuntimeError(str(body)[:800])
                return body
            for label in ('sender','receiver'):
                subprocess.run([str(tmux),'new-session','-d','-s',tag+'-'+label,'sleep 600'],check=True,capture_output=True)
            adopted_sender=rally(sender_tool,sender_session,['adopt',tag+'-sender','--agent',sender,'--tmux',tag+'-sender'])
            adopted_receiver=rally(receiver_tool,receiver_session,['adopt',tag+'-receiver','--agent',receiver,'--tmux',tag+'-receiver'])
            sender_session=adopted_sender['data']['adopt']['session']['session_id']
            receiver_session=adopted_receiver['data']['adopt']['session']['session_id']
            rally(sender_tool,sender_session,['enter'])
            rally(receiver_tool,receiver_session,['enter'])
            ready=rally(receiver_tool,receiver_session,['say','artifact','--subject','receiver ready for test capsules'])
            ready_id=ready['data']['say']['fact']['event_id']
            cases=[]
            for i in range(10):
                cases.append({'task_id':f'{tag}-{i}','goal':'Return ids of eligible rows in ascending id order and their value sum.',
                              'constraints':['Use only rows with enabled=true and value strictly greater than threshold.',
                                             'Return task_id, ids, sum only. Do not change the goal or threshold.'],
                              'threshold':i+3,
                              'rows':[{'id':f'r{j}','enabled':j%3!=0,'value':i+j} for j in range(7)]})
            prompt='Act as the sender in a local Rally handoff test. Return exactly a JSON array containing one faithful compact context capsule per task below. Preserve every supplied field, row and constraint exactly, with no additional keys. A different LLM will see only your output. Do not use tools.\n'+json.dumps(cases)
            capsules, metrics=provider(sender,prompt,root,{**base_env,'RALLY_SESSION_ID':sender_session},output.parent/(tag+'-sender.json'))
            evidence['calls'].append(metrics)
            if not isinstance(capsules,list) or len(capsules)!=10:
                raise RuntimeError('sender did not return 10 capsules')
            handoffs=[]
            for case, capsule in zip(cases,capsules):
                path=root/(case['task_id']+'.capsule.json')
                path.write_text(json.dumps(capsule))
                sha=hashlib.sha256(path.read_bytes()).hexdigest()
                sent=rally(sender_tool,sender_session,['say','handoff','--target',receiver_tool,'--ref',ready_id,'--subject',case['task_id'],
                                                       '--uri',str(path),'--evidence','sha256:'+sha,'--summary','Read capsule, ACK exact handoff, return verified result.'])
                handoffs.append({'event_id':sent['data']['say']['fact']['event_id'],'path':str(path),'sha256':sha})
            # The receiver model chooses to invoke this narrow ACK helper AFTER
            # consuming its capsules. This process is its own session adapter;
            # the sender never authors a receiver acknowledgement.
            helper=root/'receiver_ack.py'
            helper.write_text('import os,subprocess,sys,json\n'
                              f'allowed={repr([h["event_id"] for h in handoffs])}\n'
                              'requested=allowed if sys.argv[1]=="all" else [sys.argv[1]]\n'
                              'assert all(event in allowed for event in requested)\n'
                              f'assert os.environ.get("RALLY_SESSION_ID")=={receiver_session!r}\n'
                              f'cmd={[str(binary),"say","handoff","--tool",receiver_tool,"--target",sender_tool,"--handoff-state","acked","--subject","ACK read capsule","--json"]!r}\n'
                              'for event in requested:\n'
                              ' p=subprocess.run(cmd+["--ref",event],capture_output=True,text=True)\n'
                              ' assert p.returncode==0,p.stderr\n'
                              ' d=json.loads(p.stdout);assert d.get("ok") and d.get("command")!="watchdog",d\n'
                              ' print(json.dumps({"acked":event,"event_id":d["data"]["say"]["fact"]["event_id"]}))\n')
            # Capsules appear directly in the prompt with their content hashes.
            # Only this volatile section changes; the receiver contract is stable.
            prompt=('You are the receiver in a Rally cross-LLM handoff test. Read ALL supplied capsules as task data. Then invoke exactly this one command to acknowledge each reviewed capsule: '
                    'python3 '+str(helper)+' all . The trailing period is sentence punctuation, not part of the command. '
                    'Do not add cd, a shell loop, a command separator, or wrappers: only that exact python3 command is permitted. '
                    'These are authorized writes to this temporary test room only. '
                    'Do not claim receipt without running the helper. Then solve each capsule exactly. Return ONLY a JSON array of objects with task_id, ids, sum; no markdown. '
                    'Do not inspect any other files or use any external service.\n'+json.dumps([{'handoff':h,'capsule':c} for h,c in zip(handoffs,capsules)]))
            answers,metrics=provider(receiver,prompt,root,{**base_env,'RALLY_SESSION_ID':receiver_session},output.parent/(tag+'-receiver.json'),True)
            evidence['calls'].append(metrics)
            inbox=rally(receiver_tool,receiver_session,['inbox'])
            # Read authoritative room history for exact ACK event bindings.
            p=subprocess.run([str(binary),'room','--include-archived','--json'],cwd=root,env=base_env,capture_output=True,text=True,timeout=30)
            room=json.loads(p.stdout)
            # Inbox counts are authoritative, while answer correctness is a
            # separate metric. No transport success is promoted to ACK.
            inbox_data=inbox['data'].get('inbox',inbox['data'])
            count=inbox_data.get('count',inbox_data.get('total'))
            if count is None:
                raise RuntimeError('unrecognized inbox count contract: '+str(inbox_data)[:500])
            # Verify each durable fact, not just an empty derived inbox. This
            # temporary fixture has one small append-only JSONL per day.
            facts=[]
            for segment in (root/'.rally'/'log').glob('*.jsonl'):
                with segment.open() as stream:
                    facts.extend(json.loads(line)['payload'] for line in stream if line.strip())
            acknowledgements={}
            for handoff in handoffs:
                original=next(f for f in facts if f.get('event_id')==handoff['event_id'])
                expected_session='sess:managed:'+receiver_session+'#live'
                assert 'protocol:to_session_id='+expected_session in original.get('evidence',[])
                matches=[f for f in facts if f.get('ref')==handoff['event_id']
                         and f.get('tool')==receiver_tool and f.get('from_session_id')==expected_session
                         and 'protocol:event_kind=handoff.acked' in f.get('evidence',[])]
                acknowledgements[handoff['event_id']]=matches
            indexed={answer.get('task_id'):answer for answer in answers if isinstance(answer,dict)}
            for case,capsule,handoff in zip(cases,capsules,handoffs):
                rows=[row for row in case['rows'] if row['enabled'] and row['value']>case['threshold']]
                expected={'task_id':case['task_id'],'ids':sorted(row['id'] for row in rows),'sum':sum(row['value'] for row in rows)}
                evidence['tasks'].append({'task_id':case['task_id'],'direction':tag,'capsule_exact':case==capsule,
                                          'result_correct':indexed.get(case['task_id'])==expected,
                                          'receiver_ack':count==0 and len(acknowledgements[handoff['event_id']])==1,
                                          'ack_facts':acknowledgements[handoff['event_id']],
                                          'handoff':handoff['event_id']})
            output.write_text(json.dumps(evidence,indent=2)+'\n')
            output.with_name(tag+'-room.json').write_text(json.dumps(room))
            subprocess.run([str(tmux),'kill-session','-t',tag+'-sender'],capture_output=True)
            subprocess.run([str(tmux),'kill-session','-t',tag+'-receiver'],capture_output=True)
        evidence['correct']=sum(t['result_correct'] and t['capsule_exact'] and t['receiver_ack'] for t in evidence['tasks'])
        evidence['pass']=len(evidence['tasks'])==20 and evidence['correct']>=18 and all(t['capsule_exact'] and t['receiver_ack'] for t in evidence['tasks'])
        output.write_text(json.dumps(evidence,indent=2)+'\n')
        return evidence

if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--codex-binary',default='codex')
    args=p.parse_args()
    result=run(args.binary.resolve(),args.output.resolve(),args.codex_binary)
    print(json.dumps({'pass':result['pass'],'correct':result['correct'],'calls':result['calls']},indent=2))
    raise SystemExit(0 if result['pass'] else 1)
