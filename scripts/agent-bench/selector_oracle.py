"""Blind selector case construction and independent, offline task verification.

No model/device calls. Seeds and App-held oracles stay outside agent prompts.
Correct answers plus witnessed reads are task outcomes, not proof of reasoning
or a general capability/performance advantage.
"""
import hashlib
import json
import random
import re
import uuid

ROLES = [
    ('StaticText', '', True), ('StaticText', None, True),
    ('StaticText', 'stable', True), ('StaticText', ' ', True),
    ('Button', '', True), ('Button', None, True),
    ('Button', 'stable', True), ('Button', '', False),
]
BUNDLE = 'com.leeguoo.iphone-use.agent-selector-oracle'
PROMPT = '''The authorized test App is already open and healthy on an owned Simulator.
Use only the configured read tools. Do not set up, recover, launch, switch apps,
or send input. Among the exposed StaticText elements labelled Record, find the
one whose raw accessibility value is explicitly the empty string, distinct from
an absent value or a whitespace value. Return only JSON with one field:
{"identifier":"<the element's accessibility identifier>"}.
If you cannot determine it from the tools, report that rather than guessing.
'''


def make_case(seed, run_id):
    if not isinstance(seed, str) or not re.fullmatch(r'[0-9a-f]{64}', seed):
        raise ValueError('seed must be 32 private random bytes encoded as hex')
    uuid.UUID(run_id)
    rng = random.Random(int(seed, 16))
    rows = [{'identifier': 'r_' + f'{rng.getrandbits(128):032x}', 'label': 'Record',
             'kind': kind, 'value': value, 'enabled': enabled}
            for kind, value, enabled in ROLES]
    rng.shuffle(rows)
    return {'run_id': run_id, 'rows': rows}


def case_bytes(case):
    return (json.dumps(case, sort_keys=True, separators=(',', ':')) + '\n').encode()


def case_digest(case):
    return hashlib.sha256(case_bytes(case)).hexdigest()


def validate_rows(rows):
    if not isinstance(rows, list) or len(rows) != len(ROLES):
        raise ValueError('expected eight App rows')
    ids = set()
    roles = []
    for row in rows:
        if not isinstance(row, dict) or set(row) != {'identifier', 'label', 'kind', 'value', 'enabled'}:
            raise ValueError('invalid App row fields')
        identifier = row['identifier']
        if not isinstance(identifier, str) or not re.fullmatch(r'r_[0-9a-f]{32}', identifier) or identifier in ids:
            raise ValueError('row identifiers must be unique opaque tokens')
        ids.add(identifier)
        if row['label'] != 'Record' or type(row['enabled']) is not bool:
            raise ValueError('invalid neutral row label or enabled state')
        roles.append((row['kind'], row['value'], row['enabled']))
    # Compare small multisets without assuming values are hashable or ordering
    # None against strings. Malformed values fail cleanly.
    remaining = list(ROLES)
    for role in roles:
        if role not in remaining:
            raise ValueError('row role differs from the declared case family')
        remaining.remove(role)


def target_identifier(rows):
    validate_rows(rows)
    return next(row['identifier'] for row in rows
                if row['kind'] == 'StaticText' and row['value'] == '')


def _witness_rows(value):
    """Recognize MCP content envelopes and top-level row collections only."""
    if isinstance(value, str):
        try:
            yield from _witness_rows(json.loads(value))
        except (ValueError, RecursionError):
            pass
    elif isinstance(value, list):
        for block in value:
            if isinstance(block, dict) and block.get('type') == 'text':
                yield from _witness_rows(block.get('text'))
    elif isinstance(value, dict):
        if value.get('error') is not None or value.get('isError') is True or value.get('ok') is False:
            return
        for key in ('elements', 'nodes'):
            if isinstance(value.get(key), list):
                yield from (row for row in value[key] if isinstance(row, dict))
        if isinstance(value.get('structuredContent'), dict):
            yield from _witness_rows(value['structuredContent'])
        if isinstance(value.get('content'), list):
            yield from _witness_rows(value['content'])
        if value.get('jsonrpc') == '2.0' and isinstance(value.get('result'), dict):
            yield from _witness_rows(value['result'])


def witnessed_identifier(events, identifier, read_tools):
    """Require one uniquely correlated successful read returning this ID."""
    calls, seen, invalid = {}, set(), set()
    witnessed, results_seen = set(), set()
    for event in events:
        if not isinstance(event, dict):
            continue
        message = event.get('message')
        if not isinstance(message, dict) or not isinstance(message.get('content'), list):
            continue
        for block in message['content']:
            if not isinstance(block, dict):
                continue
            if event.get('type') == 'assistant' and block.get('type') == 'tool_use':
                ident = block.get('id')
                if not isinstance(ident, str) or not ident:
                    continue
                if ident in seen:
                    invalid.add(ident)
                seen.add(ident)
                calls[ident] = block.get('name')
            elif event.get('type') == 'user' and block.get('type') == 'tool_result':
                ident = block.get('tool_use_id')
                if not isinstance(ident, str):
                    continue
                if ident in results_seen:
                    return False
                results_seen.add(ident)
                if calls.get(ident) not in read_tools or block.get('is_error'):
                    continue
                if any(row.get('identifier', row.get('name')) == identifier
                       for row in _witness_rows(block.get('content'))):
                    witnessed.add(ident)
    return bool(witnessed - invalid)


def verify(case, model, before, after, answer, events, read_tools, *, active_before, active_after):
    """Return failures. The caller separately binds trace/context/proof hashes."""
    validate_rows(case['rows'])
    validate_rows(model['rows'])
    errors = []
    for active in [active_before, active_after]:
        if not isinstance(active, dict) or active.get('bundleId') != BUNDLE or type(active.get('pid')) is not int or active['pid'] != before.get('pid'):
            errors.append('live App identity changed or missing')
    digest = case_digest(case)
    if model['rows'] != case['rows']:
        errors.append('App properties differ from the supplied case')
    for state in [model, before, after]:
        if state.get('run_id') != case['run_id'] or state.get('case_sha256') != digest:
            errors.append('stale or different App case')
    if type(before.get('pid')) is not int or before['pid'] <= 0 or type(after.get('pid')) is not int or after.get('pid') != before['pid']:
        errors.append('App process changed or missing')
    for state in [before, after]:
        if any(type(state.get(key)) is not int or state[key] != 0 for key in ('input_events', 'foreground_losses', 'button_activations')):
            errors.append('fixture received input or lost foreground')
        if type(state.get('application_state')) is not int or state['application_state'] != 0 or state.get('key_window') is not True:
            errors.append('App not active or unexpected input')
    try:
        def unique_object(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError('duplicate answer field')
                result[key] = value
            return result
        parsed = json.loads(answer, object_pairs_hook=unique_object)
    except (TypeError, ValueError):
        parsed = None
    expected = target_identifier(model['rows'])
    if not isinstance(parsed, dict) or set(parsed) != {'identifier'} or parsed['identifier'] != expected:
        errors.append('answer differs from independent App target')
    if not witnessed_identifier(events, expected, read_tools):
        errors.append('target not witnessed in a successful backend read')
    return errors
