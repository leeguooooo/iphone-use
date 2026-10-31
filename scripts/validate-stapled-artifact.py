#!/usr/bin/env python3
"""Fail closed on invalid tickets; retry only observed transient Apple network errors."""
import argparse
import re
import subprocess
import time
NETWORK=re.compile(r'NSURLErrorDomain Code=-(?:1001|1003|1004|1005|1009)\b')
def validate(app,run=subprocess.run,sleep=time.sleep):
    for attempt in range(1,4):
        result=run(['xcrun','stapler','validate',str(app)],capture_output=True,text=True,timeout=120)
        output=result.stdout+result.stderr
        print(output,end='' if output.endswith('\n') else '\n')
        if result.returncode==0:return 0
        if result.returncode!=68 or not NETWORK.search(output) or attempt==3:
            return result.returncode if result.returncode>0 else 1
        print(f'Apple network validation failed; retrying this read only ({attempt}/3)')
        sleep(2)
    raise AssertionError('unreachable')
if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('app');args=parser.parse_args()
    raise SystemExit(validate(args.app))
