"""Run with python3 -m unittest discover -s scripts -p 'test_mcp_stdio.py'."""

import importlib.util
import io
import json
from pathlib import Path
import threading
import unittest
from unittest.mock import Mock, patch
from http.server import BaseHTTPRequestHandler, HTTPServer

spec = importlib.util.spec_from_file_location(
    "mcp_stdio", Path(__file__).with_name("mcp_stdio.py")
)
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class BridgeTests(unittest.TestCase):
    def test_environment_names_keep_legacy_compatibility_and_new_name_priority(self):
        for name in ('URL', 'TOKEN', 'ALLOW_HTTP'):
            with self.subTest(name=name):
                with patch.dict(bridge.os.environ, {'AI_EMAIL_' + name: 'legacy'}, clear=True):
                    self.assertEqual(bridge.env_setting(name), 'legacy')
                    bridge.os.environ['MAILORVA_' + name] = 'new'
                    self.assertEqual(bridge.env_setting(name), 'new')
                    bridge.os.environ['MAILORVA_' + name] = ''
                    self.assertEqual(bridge.env_setting(name), '')
                with patch.dict(bridge.os.environ, {}, clear=True):
                    self.assertEqual(bridge.env_setting(name), '')

    def test_rejects_credentials_in_url_and_remote_cleartext(self):
        for url in ("http://example.com/mcp", "https://user:pass@example.com/mcp",
                    "https://example.com/mcp?token=secret", "file:///tmp/mcp"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                bridge.validate_url(url, allow_http=True)
        self.assertEqual(bridge.validate_url("https://example.com/mcp"),
                         "https://example.com/mcp")

    def test_http_loopback_requires_explicit_development_opt_in(self):
        with self.assertRaises(ValueError):
            bridge.validate_url("http://127.0.0.1:8080/mcp")
        self.assertEqual(bridge.validate_url("http://127.0.0.1:8080/mcp", True),
                         "http://127.0.0.1:8080/mcp")

    def test_sse_handles_comments_and_multiline_data(self):
        raw = b': heartbeat\r\nevent: message\r\ndata: {"jsonrpc":"2.0",\r\ndata: "id":1,"result":{}}\r\n\r\n'
        self.assertEqual(list(bridge.read_sse(io.BytesIO(raw))),
                         [{"jsonrpc": "2.0", "id": 1, "result": {}}])

    def test_oversized_sse_event_is_rejected(self):
        with self.assertRaises(ValueError):
            list(bridge.read_sse(io.BytesIO(b'data: ' + b'x' * 100 + b'\n\n'), max_bytes=32))

    def test_complete_large_mail_response_is_not_limited_to_request_size(self):
        # 服务接受的正常正文超过原桥接 16 MiB 上限；验证实际 HTTP 解析路径。
        body = 'x' * (17 * 1024 * 1024)
        wire = json.dumps({'jsonrpc': '2.0', 'id': 1, 'result': {'body_text': body}}).encode()
        response = io.BytesIO(wire)
        response.headers = {'Content-Type': 'application/json'}
        response.status = 200
        client = bridge.HttpBridge('https://example.com/mcp', 'token')
        client.opener = Mock()
        client.opener.open.return_value = response
        messages = list(client.forward({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
                                        'params': {'name': 'get_message', 'arguments': {'id': 'mail'}}}))
        self.assertEqual(len(messages[0]['result']['body_text']), len(body))


class TransportTests(unittest.TestCase):
    def setUp(self):
        self.requests = []
        requests = self.requests

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                requests.append((self.path, dict(self.headers), request))
                if self.path == '/redirect':
                    self.send_response(307)
                    self.send_header('Location', '/leaked')
                    self.end_headers()
                    return
                if 'id' not in request:
                    self.send_response(202)
                    self.end_headers()
                    return
                self.send_response(200)
                if request['method'] == 'initialize':
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Mcp-Session-Id', 'test-session')
                    self.end_headers()
                    self.wfile.write(json.dumps({'jsonrpc': '2.0', 'id': request['id'],
                        'result': {'protocolVersion': '2025-11-25', 'capabilities': {'tools': {}},
                                   'serverInfo': {'name': 'test', 'version': '1'}}}).encode())
                else:
                    self.send_header('Content-Type', 'text/event-stream')
                    self.end_headers()
                    response = {'jsonrpc': '2.0', 'id': request['id'], 'result': {'tools': []}}
                    self.wfile.write(('data: ' + json.dumps(response) + '\n\n').encode())

            def log_message(self, *args):
                pass

        self.server = HTTPServer(('127.0.0.1', 0), Handler)
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}'

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join()

    def test_json_sse_session_headers_and_notification_forwarding(self):
        client = bridge.HttpBridge(self.url + '/mcp', 'a-secret-token', allow_http=True)
        responses = list(client.forward({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize',
                                         'params': {'protocolVersion': '2025-11-25'}}))
        self.assertEqual(responses[0]['result']['protocolVersion'], '2025-11-25')
        self.assertEqual(list(client.forward({'jsonrpc': '2.0', 'method': 'notifications/initialized'})), [])
        self.assertEqual(list(client.forward({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list'})),
                         [{'jsonrpc': '2.0', 'id': 2, 'result': {'tools': []}}])
        headers = {key.lower(): value for key, value in self.requests[2][1].items()}
        self.assertEqual(headers['authorization'], 'Bearer a-secret-token')
        self.assertEqual(headers['mcp-session-id'], 'test-session')
        self.assertEqual(headers['mcp-protocol-version'], '2025-11-25')

    def test_redirect_is_not_followed_with_secret(self):
        client = bridge.HttpBridge(self.url + '/redirect', 'secret', allow_http=True)
        with self.assertRaises(Exception):
            list(client.forward({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}))
        self.assertEqual([item[0] for item in self.requests], ['/redirect'])


if __name__ == '__main__':
    unittest.main()
