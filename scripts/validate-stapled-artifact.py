#!/usr/bin/env python3
"""Validate or staple an owned bundle; retry only explicit transient ticket-fetch errors."""
import argparse
import re
import subprocess
import time
NETWORK=re.compile(r'NSURLErrorDomain Code=-(?:1001|1003|1004|1005|1009)\b')
def ticket_action(operation,app,run=subprocess.run,sleep=time.sleep):
    assert operation in ("validate","staple")
    for attempt in range(1,4):
        result=run(['xcrun','stapler',operation,str(app)],capture_output=True,text=True,timeout=120)
        output=result.stdout+result.stderr
        print(output,end='' if output.endswith('\n') else '\n')
        if result.returncode==0:return 0
        if result.returncode!=68 or not NETWORK.search(output) or attempt==3:
            return result.returncode if result.returncode>0 else 1
        print(f'Apple ticket network request failed; retrying {operation} on the same bundle ({attempt}/3)')
        sleep(2)
    raise AssertionError('unreachable')
def validate(app,run=subprocess.run,sleep=time.sleep):
    return ticket_action('validate',app,run,sleep)
def staple(app,run=subprocess.run,sleep=time.sleep):
    return ticket_action('staple',app,run,sleep)
if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('app');parser.add_argument('--staple',action='store_true');args=parser.parse_args()
    raise SystemExit((staple if args.staple else validate)(args.app))
