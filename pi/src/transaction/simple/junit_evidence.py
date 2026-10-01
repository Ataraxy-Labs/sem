"""Bounded, read-only JUnit failure details. Never determines a check verdict."""
import xml.etree.ElementTree as ET

MAX_REPORT_BYTES = 2 * 1024 * 1024

def assertion_difference(message):
    """Excerpt a standard JUnit string assertion; ambiguous formats stay raw."""
    marker='> but was: <'
    start=message.find('expected: <')
    if start<0 or message.count(marker)!=1 or not message.endswith('>'):
        return None
    expected,actual=message[start+len('expected: <'):-1].split(marker)
    if expected==actual:
        return None
    offset=0
    for left,right in zip(expected,actual):
        if left!=right:
            break
        offset+=1
    begin=max(0,offset-120)
    return {'first_difference_character':offset,'window_start_character':begin,
            'expected':expected[begin:offset+240],'actual':actual[begin:offset+240],
            'coverage':'First string difference only; other differences may exist.'}

def parse_report(payload, source, limit=10):
    if not isinstance(payload, bytes) or len(payload)>MAX_REPORT_BYTES:
        raise ValueError('Report exceeds byte bound')
    if not 1<=limit<=50:
        raise ValueError('Invalid report page size')
    if b'<!DOCTYPE' in payload.upper() or b'<!ENTITY' in payload.upper():
        raise ValueError('DTD/entity declarations are not supported')
    root=ET.fromstring(payload)
    failures=[]
    groups={}
    total=0
    for case in root.iter('testcase'):
        for child in case:
            if child.tag not in ('failure','error'):
                continue
            total+=1
            message=child.get('message','')
            detail=child.text or ''
            identity=(case.get('classname',''),child.tag,message,detail)
            if identity in groups:
                group=groups[identity]
                group['occurrences']+=1
                if len(group['tests'])<5:
                    group['tests'].append(case.get('name','')[:500])
                else:
                    group['omitted_test_names']+=1
                continue
            if len(failures)>=limit:
                continue
            entry={
                'test':case.get('name','')[:500],
                'tests':[case.get('name','')[:500]],
                'occurrences':1,'omitted_test_names':0,
                'class':case.get('classname','')[:500],
                'kind':child.tag,
                'message':message[:1500],
                'detail':detail[:2500],
                'assertion_difference':assertion_difference(message),
                'truncated':len(message)>1500 or len(detail)>2500,
            }
            failures.append(entry)
            groups[identity]=entry
    return {'source':source,'failures':failures,'total_failure_elements':total,
            'omitted_failures':total-sum(item['occurrences'] for item in failures),
            'coverage':'This report only; not a validation verdict or proof of freshness.'}
