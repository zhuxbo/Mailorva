"""真实服务进程与 stdio 桥接的本机冒烟；不会连接真实邮箱或执行 SMTP。"""

import datetime
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
BINARY = Path(os.environ.get('AI_EMAIL_TEST_BINARY', ROOT / 'server/target/debug/ai-email-server'))
READ_TOKEN = 'r' * 64
WRITE_TOKEN = 'w' * 64


class ServerSmokeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BINARY.is_file():
            raise RuntimeError('先构建服务，或设置 AI_EMAIL_TEST_BINARY 为服务二进制路径')
        cls.temp = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.temp.cleanup)
        folder = Path(cls.temp.name)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        cls.url = f'http://127.0.0.1:{port}'
        for name, value in [('read', READ_TOKEN), ('write', WRITE_TOKEN), ('password', 'test-only')]:
            (folder / name).write_text(value)
        config = {'bind': f'127.0.0.1:{port}', 'database_path': str(folder / 'mail.db'),
                  'read_token': {'file': str(folder / 'read')},
                  'write_token': {'file': str(folder / 'write')},
                  'accounts': [{'id': 'work', 'email': 'work@example.com', 'display_name': '工作',
                                'imap_host': '127.0.0.1', 'imap_port': 1,
                                'smtp_host': '127.0.0.1', 'smtp_port': 1,
                                'password': {'file': str(folder / 'password')}, 'folders': ['INBOX']}]}
        path = folder / 'config.json'
        path.write_text(json.dumps(config))
        cls.server = subprocess.Popen([str(BINARY), str(path)], stdout=subprocess.DEVNULL,
                                      stderr=subprocess.DEVNULL)
        cls.addClassCleanup(cls.stop_server)
        for _ in range(100):
            if cls.server.poll() is not None:
                raise RuntimeError('测试服务提前退出')
            try:
                cls.request('/api/accounts')
                break
            except urllib.error.URLError:
                time.sleep(0.05)
        else:
            raise RuntimeError('测试服务启动超时')
        received = int(time.time())
        cls.message = {'id': 'work:494e424f58:1:1', 'account_id': 'work',
                       'account_email': 'work@example.com', 'folder': 'INBOX', 'subject': '证书到期',
                       'from_address': 'vendor@example.com', 'to_addresses': ['work@example.com'],
                       'cc_addresses': [], 'received_at': datetime.datetime.fromtimestamp(
                           received, datetime.timezone.utc).isoformat(),
                       'body_preview': '订单 100%_完成', 'body_text': '订单 100%_完成',
                       'message_id': '<fixture@example.com>', 'references': [], 'flags': [], 'attachments': []}
        with sqlite3.connect(folder / 'mail.db', timeout=5) as db:
            db.execute('INSERT INTO messages VALUES(?,?,?,?,?,?,?,?)',
                       (cls.message['id'], 'work', 'INBOX', 1, 1, received,
                        '证书到期\n订单 100%_完成\nwork@example.com\nvendor@example.com',
                        json.dumps(cls.message)))

    @classmethod
    def stop_server(cls):
        cls.server.terminate()
        try:
            cls.server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            cls.server.kill()
            cls.server.wait()
            raise AssertionError('服务没有及时响应 SIGTERM')
        if cls.server.returncode != 0:
            raise AssertionError(f'服务退出码异常: {cls.server.returncode}')

    @classmethod
    def request(cls, path, token=READ_TOKEN, payload=None, origin=None, method=None):
        headers = {'Authorization': 'Bearer ' + token}
        if origin:
            headers['Origin'] = origin
        if payload is not None:
            headers['Content-Type'] = 'application/json'
        request = urllib.request.Request(cls.url + path, headers=headers, method=method,
            data=None if payload is None else json.dumps(payload).encode())
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    def test_android_read_api_and_auth_boundaries(self):
        self.assertEqual(self.request('/api/accounts')['accounts'][0]['id'], 'work')
        query = urllib.parse.urlencode({'q': '证书 100%_', 'account_id': 'work', 'limit': 30})
        result = self.request('/api/messages?' + query)
        self.assertEqual(result['total'], 1)
        self.assertNotIn('body_text', result['items'][0])
        message = self.request('/api/messages/' + urllib.parse.quote(self.message['id'], safe=''))
        self.assertEqual(message['body_text'], '订单 100%_完成')
        for path, kwargs, status in [('/api/accounts', {'token': 'wrong'}, 401),
                                    ('/api/accounts', {'origin': 'https://untrusted.example'}, 403),
                                    ('/api/messages/any/flags', {'payload': {'seen': True}}, 403)]:
            with self.subTest(path=path, status=status), self.assertRaises(urllib.error.HTTPError) as error:
                self.request(path, **kwargs)
            self.assertEqual(error.exception.code, status)

    def run_bridge(self, token, calls, version='2025-11-25'):
        modern = version >= '2026-07-28'
        metadata = {'io.modelcontextprotocol/protocolVersion': version,
                    'io.modelcontextprotocol/clientCapabilities': {},
                    'io.modelcontextprotocol/clientInfo': {'name': 'smoke-test', 'version': '1'}}
        messages = [{'jsonrpc': '2.0', 'id': 0, 'method': 'server/discover',
                     'params': {'_meta': metadata}}] if modern else [
                    {'jsonrpc': '2.0', 'id': 0, 'method': 'initialize',
                     'params': {'protocolVersion': version, 'capabilities': {},
                                'clientInfo': {'name': 'smoke-test', 'version': '1'}}},
                    {'jsonrpc': '2.0', 'method': 'notifications/initialized'}]
        for index, (method, params) in enumerate(calls, 1):
            if modern:
                params = {**params, '_meta': metadata}
            messages.append({'jsonrpc': '2.0', 'id': index, 'method': method, 'params': params})
        completed = subprocess.run([sys.executable, str(ROOT / 'scripts/mcp_stdio.py')],
            env={**os.environ, 'AI_EMAIL_URL': self.url + '/mcp', 'AI_EMAIL_TOKEN': token,
                 'AI_EMAIL_ALLOW_HTTP': '1'},
            input=''.join(json.dumps(m) + '\n' for m in messages), text=True, capture_output=True, timeout=20)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stderr, '')
        result = [json.loads(line) for line in completed.stdout.splitlines()]
        self.assertEqual(len(result), len(calls) + 1)
        self.assertIn('result', result[0])
        if modern:
            self.assertIn(version, result[0]['result']['supportedVersions'])
        else:
            self.assertEqual(result[0]['result']['protocolVersion'], version)
        return result[1:]

    def test_stdio_mcp_search_and_readonly_write_denial(self):
        results = self.run_bridge(READ_TOKEN, [('tools/list', {}),
            ('tools/call', {'name': 'search', 'arguments': {'q': '订单 work@example.com'}}),
            ('tools/call', {'name': 'get_message', 'arguments': {'id': self.message['id']}}),
            ('tools/call', {'name': 'set_flags', 'arguments': {'id': self.message['id'], 'seen': True}})])
        self.assertIn('send_prepared', {tool['name'] for tool in results[0]['result']['tools']})
        self.assertEqual(json.loads(results[1]['result']['content'][0]['text'])['total'], 1)
        self.assertEqual(json.loads(results[2]['result']['content'][0]['text'])['body_text'], '订单 100%_完成')
        self.assertTrue(results[3]['result']['isError'])
        self.assertEqual(json.loads(results[3]['result']['content'][0]['text'])['error']['code'], 'unauthorized')

    def test_write_token_prepares_and_checks_without_sending(self):
        draft = {'operation_id': 'smoke-no-send', 'account_id': 'work', 'to': ['never-send@example.com'],
                 'subject': '仅测试准备', 'body_text': '不会调用 SMTP'}
        results = self.run_bridge(WRITE_TOKEN, [
            ('tools/call', {'name': 'prepare_send', 'arguments': draft}),
            ('tools/call', {'name': 'get_send_status', 'arguments': {'operation_id': 'smoke-no-send'}})])
        for response in results:
            self.assertEqual(json.loads(response['result']['content'][0]['text'])['status'], 'prepared')

    def test_current_protocol_can_list_accounts(self):
        results = self.run_bridge(READ_TOKEN, [
            ('tools/call', {'name': 'list_accounts', 'arguments': {}})], version='2026-07-28')
        self.assertEqual(json.loads(results[0]['result']['content'][0]['text'])['accounts'][0]['id'], 'work')

    def test_native_reply_preview_permissions_and_server_derived_recipient(self):
        self.assertFalse(self.request('/api/accounts')['can_write'])
        self.assertTrue(self.request('/api/accounts', token=WRITE_TOKEN)['can_write'])
        route = '/api/messages/' + urllib.parse.quote(self.message['id'], safe='') + '/reply/prepare'
        body = {'operation_id': 'native-smoke-no-send', 'body_text': '收到，谢谢。'}
        with self.assertRaises(urllib.error.HTTPError) as error:
            self.request(route, payload=body)
        self.assertEqual(error.exception.code, 403)
        preview = self.request(route, token=WRITE_TOKEN, payload=body)
        self.assertEqual(preview['account_id'], 'work')
        self.assertEqual(preview['account_email'], 'work@example.com')
        self.assertEqual(preview['to'], ['vendor@example.com'])
        self.assertEqual(preview['subject'], 'Re: 证书到期')
        self.assertEqual(preview['body_text'], body['body_text'])
        self.assertEqual(preview['status'], 'prepared')
        self.assertEqual(self.request(route, token=WRITE_TOKEN, payload=body), preview)

    def test_manage_multiple_accounts_without_touching_remote_mail(self):
        route = '/api/account-settings'
        for method in ['GET', 'POST']:
            with self.assertRaises(urllib.error.HTTPError) as error:
                self.request(route, method=method, payload={} if method == 'POST' else None)
            self.assertEqual(error.exception.code, 403)
        created = []
        try:
            for index in range(2):
                draft = {'email': f'managed{index}@example.com', 'display_name': f'测试 {index}',
                         'imap_host': '127.0.0.1', 'imap_port': 1,
                         'smtp_host': '127.0.0.1', 'smtp_port': 1, 'folders': ['INBOX'],
                         'password': 'test-only'}
                account = self.request(route, token=WRITE_TOKEN, payload=draft)
                created.append(account['id'])
                self.assertNotIn('password', account)
                self.assertNotIn('credential', account)
            self.assertEqual(len(set(created)), 2)
            rows = self.request(route, token=WRITE_TOKEN)['accounts']
            self.assertTrue(set(created).issubset({a['id'] for a in rows}))
            row = next(a for a in rows if a['id'] == created[0])
            row.pop('id')
            row['display_name'] = '已更新'
            row['password'] = None
            updated = self.request(route + '/' + created[0], token=WRITE_TOKEN,
                                   payload=row, method='PUT')
            self.assertEqual(updated['display_name'], '已更新')
            with self.assertRaises(urllib.error.HTTPError) as error:
                self.request(route + '/' + created[0], method='DELETE')
            self.assertEqual(error.exception.code, 403)
        finally:
            for account_id in created:
                self.assertTrue(self.request(route + '/' + account_id, token=WRITE_TOKEN,
                                             method='DELETE')['deleted'])
        self.assertEqual([a['id'] for a in self.request(route, token=WRITE_TOKEN)['accounts']], ['work'])


if __name__ == '__main__':
    unittest.main()
