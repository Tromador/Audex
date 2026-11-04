"""Offline control replies for the production pre-prompt boundary tests."""
import json
import os
from pathlib import Path
import sys
import time


def hold(fixture, name):
    (fixture / (name + '.ready')).write_text(json.dumps({'pid': os.getpid()}))
    deadline = time.monotonic() + 10
    while not (fixture / (name + '.release')).exists():
        assert time.monotonic() < deadline, 'policy fixture gate timeout'
        time.sleep(0.005)


def refresh_credential(fixture):
    # Atomic replacement of dummy bytes in the owned test profile only.
    replacement = fixture / 'dummy-refreshed-credential'
    replacement.write_text('dummy-refresh-' + str(os.getpid()))
    replacement.chmod(0o600)
    replacement.replace(Path(os.environ['CLAUDE_CONFIG_DIR'], '.credentials.json'))


def handshake():
    config = json.loads(Path(sys.argv[sys.argv.index('--mcp-config') + 1]).read_text())
    args = config['mcpServers']['audex_source']['args']
    state_root = Path(args[args.index('--state-root') + 1])
    fixture = Path(sys.argv[0]).parent
    scenario = json.loads((fixture / 'policy-scenario.json').read_text()) if (fixture / 'policy-scenario.json').exists() else {}
    settings = json.loads(Path(os.environ['CLAUDE_CONFIG_DIR'], 'settings.json').read_text())
    managed = scenario.get('managed_settings', {'forceRemoteSettingsRefresh': True})
    effective = {**settings, **managed}
    mode = scenario.get('mode', 'valid')
    for number, subtype in enumerate(['initialize', 'get_usage', 'get_settings', 'get_hooks_listing', 'get_status']):
        request = json.loads(sys.stdin.buffer.readline())
        assert request['type'] == 'control_request'
        assert request['request']['subtype'] == subtype
        (fixture / 'policy-control-count').write_text(str(number + 1))
        if mode == 'timeout':
            time.sleep(30)
        if mode == 'early_eof':
            sys.exit(0)
        if mode == 'malformed':
            print('{invalid', flush=True)
            continue
        if mode == 'oversized':
            print('x' * (256 * 1024 + 1), flush=True)
            continue
        if subtype == 'initialize':
            response = {'pid': os.getpid(), 'claude_code_version': '2.1.314', 'current_permission_mode': 'dontAsk',
                        'commands': [], 'output_style': 'default', 'account': {'apiProvider': 'firstParty', 'subscriptionType': scenario.get('display_tier', 'Claude Max')}}
            if mode == 'gateway':
                response['account']['apiProvider'] = 'gateway'
            if mode == 'wrong_client_pid':
                response['pid'] += 1
            if mode == 'wrong_client_version':
                response['claude_code_version'] = '999.0.0'
            if mode == 'api_account':
                response['account']['apiKeySource'] = 'ANTHROPIC_API_KEY'
        elif subtype == 'get_usage':
            assert request['request']['skip_behaviors'] is True
            if scenario.get('refresh_credential'):
                refresh_credential(fixture)
            if mode == 'policy_changed_during_usage':
                Path(os.environ['CLAUDE_CONFIG_DIR'], 'settings.json').write_text(json.dumps({**settings, 'theme':'light'}))
            if scenario.get('hold_usage'):
                hold(fixture, 'usage')
            response = {'subscription_type': scenario.get('tier', 'max')}
            if mode in ['team_tier', 'enterprise_tier', 'unknown_tier', 'null_tier']:
                response['subscription_type'] = {'team_tier':'team', 'enterprise_tier':'enterprise',
                                                 'unknown_tier':'unknown', 'null_tier':None}[mode]
            if mode == 'missing_tier':
                response = {}
        elif subtype == 'get_settings':
            # Receipt of this request proves Audex finished get_usage and bound
            # this reviewer's credential metadata before sending it.
            if mode == 'credential_changed_after_usage':
                refresh_credential(fixture)
            if scenario.get('hold_settings'):
                hold(fixture, 'settings')
            response = {'effective': effective, 'sources': [{'source': 'userSettings', 'settings': settings},
                        {'source': 'policySettings', 'settings': managed}],
                        'applied': {'model': 'claude-haiku-5-5', 'advisor': None, 'ultracode': False, 'ultracodeRequested': False}}
            if not managed:
                response['sources'] = response['sources'][:1]
            if mode == 'missing_user':
                response['sources'] = response['sources'][1:]
            if mode == 'missing_authority':
                response['sources'] = response['sources'][:1]
            if mode == 'wrong_model':
                response['applied']['model'] = 'unqualified-model'
            if mode == 'memory_enabled':
                response['effective']['autoMemoryEnabled'] = True
            if mode == 'parent':
                response['sources'].append({'source': 'flagSettings', 'settings': {}})
            if mode == 'load_error':
                response['errors'] = [{'message': 'PRIVATE_SENTINEL_MUST_NOT_PERSIST'}]
            if mode == 'remote_env':
                response['effective']['env'] = {'ANTHROPIC_API_KEY': 'PRIVATE_SENTINEL_MUST_NOT_PERSIST'}
        elif subtype == 'get_hooks_listing':
            response = {'events': [], 'hooks': [], 'policy': {'allDisabled': True, 'policyHookCount': 0}}
            if mode == 'managed_hook':
                response['policy']['policyHookCount'] = 1
        else:
            rows = [{'label': 'Setting sources', 'value': scenario.get('setting_source', 'User settings, Enterprise managed settings (file)')}]
            if mode in ['remote_pending', 'remote_stale', 'remote_failed']:
                rows.append({'label': 'Managed settings (remote)', 'value': mode})
            if mode == 'remote_source':
                rows[0]['value'] = 'User settings, Enterprise managed settings (remote)'
            if mode == 'organisation_policy':
                rows.append({'label':'Organization policy','value':'pending'})
            if mode == 'windows_policy':
                rows[0]['value'] = 'User settings, Enterprise managed settings (HKLM)'
            response = {'sections': [{'title': 'Environment', 'rows': rows}]}
        ident = request['request_id'] if mode != 'wrong_response_id' else 'foreign'
        print(json.dumps({'type': 'control_response', 'response': {'subtype': 'success', 'request_id': ident, 'response': response}}), flush=True)
    return sys.stdin.buffer.readline()
