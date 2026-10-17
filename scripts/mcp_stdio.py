#!/usr/bin/env python3
"""将标准 MCP stdio 消息转发到远程 Streamable HTTP，不依赖特定 AI 客户端。"""

import ipaddress
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

MAX_REQUEST_BYTES = 2 * 1024 * 1024
# 原信最多 25 MiB；MCP text 内嵌 JSON 对控制字符可能产生两层转义。
MAX_RESPONSE_BYTES = 256 * 1024 * 1024


def validate_url(url, allow_http=False):
    parsed = urllib.parse.urlsplit(url)
    if not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError("服务地址不能包含凭据、查询参数或片段")
    if parsed.scheme == 'https':
        return url
    try:
        local = ipaddress.ip_address(parsed.hostname).is_loopback
    except ValueError:
        local = parsed.hostname == 'localhost'
    if parsed.scheme == 'http' and allow_http and local:
        return url
    raise ValueError("远程服务必须使用 HTTPS；仅允许显式启用的本机 HTTP 开发地址")


def read_sse(stream, max_bytes=MAX_RESPONSE_BYTES):
    data = []
    size = 0
    while True:
        line = stream.readline(max_bytes + 1)
        if len(line) > max_bytes:
            raise ValueError("MCP 响应过大")
        if not line:
            break
        size += len(line)
        if size > max_bytes:
            raise ValueError("MCP 响应过大")
        line = line.decode('utf-8').rstrip('\r\n')
        if not line:
            if data:
                yield json.loads('\n'.join(data))
                data = []
            continue
        if line.startswith('data:'):
            value = line[5:]
            data.append(value[1:] if value.startswith(' ') else value)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        # 不将 Authorization 跟随重定向发送到另一台服务器。
        return None


class HttpBridge:
    def __init__(self, url, token, allow_http=False):
        self.url = validate_url(url, allow_http)
        if not token or any(ord(char) < 32 or ord(char) > 126 for char in token):
            raise ValueError("请通过 MAILORVA_TOKEN 设置有效的访问令牌")
        self.token = token
        self.session = None
        self.version = None
        self.opener = urllib.request.build_opener(NoRedirect())

    def forward(self, message):
        headers = {'Authorization': 'Bearer ' + self.token,
                   'Content-Type': 'application/json',
                   'Accept': 'application/json, text/event-stream'}
        if self.session:
            headers['Mcp-Session-Id'] = self.session
        if self.version:
            headers['MCP-Protocol-Version'] = self.version
        # 新协议的元数据镜像；旧版客户端仍按其初始化协商结果转发。
        meta = message.get('params', {}).get('_meta', {})
        protocol = meta.get('io.modelcontextprotocol/protocolVersion')
        if protocol:
            headers['MCP-Protocol-Version'] = protocol
        if 'method' in message:
            headers['Mcp-Method'] = message['method']
        name = message.get('params', {}).get('name') or message.get('params', {}).get('uri')
        if isinstance(name, str) and name.isascii():
            headers['Mcp-Name'] = name
        request = urllib.request.Request(self.url, json.dumps(message).encode('utf-8'), headers)
        with self.opener.open(request, timeout=180) as response:
            session = response.headers.get('Mcp-Session-Id')
            if session:
                self.session = session
            if response.status in (202, 204):
                return
            content_type = response.headers.get('Content-Type', '').split(';', 1)[0].strip()
            if content_type == 'text/event-stream':
                messages = read_sse(response)
            elif content_type == 'application/json':
                raw = response.read(MAX_RESPONSE_BYTES + 1)
                if len(raw) > MAX_RESPONSE_BYTES:
                    raise ValueError("MCP 响应过大")
                messages = [json.loads(raw)]
            else:
                raise ValueError("服务器返回了不支持的 MCP 响应类型")
            for item in messages:
                if not isinstance(item, dict) or item.get('jsonrpc') != '2.0':
                    raise ValueError("服务器返回了无效的 MCP 消息")
                if message.get('method') == 'initialize' and item.get('id') == message.get('id'):
                    self.version = item.get('result', {}).get('protocolVersion')
                yield item
                # 工具最终结果收到后关闭请求流，不依赖反代立即关闭 SSE。
                if 'id' in message and item.get('id') == message['id'] and ('result' in item or 'error' in item):
                    break


def write_message(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=False) + '\n')
    sys.stdout.flush()


def env_setting(name):
    # 优先使用新名称；未配置时兼容已有客户端的环境变量。
    return os.environ.get('MAILORVA_' + name, os.environ.get('AI_EMAIL_' + name, ''))


def main():
    try:
        client = HttpBridge(env_setting('URL'),
                            env_setting('TOKEN'),
                            env_setting('ALLOW_HTTP') == '1')
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2
    while True:
        line = sys.stdin.buffer.readline(MAX_REQUEST_BYTES + 1)
        if not line:
            return 0
        if len(line) > MAX_REQUEST_BYTES:
            print('MCP 请求过大，停止桥接', file=sys.stderr)
            return 2
        try:
            message = json.loads(line)
            if not isinstance(message, dict) or message.get('jsonrpc') != '2.0':
                raise ValueError('无效 JSON-RPC 消息')
        except (ValueError, UnicodeError):
            write_message({'jsonrpc': '2.0', 'id': None,
                           'error': {'code': -32700, 'message': '无效 JSON-RPC 消息'}})
            continue
        try:
            for response in client.forward(message):
                write_message(response)
        except Exception as error:
            # 服务响应与网络错误可能含认证信息或邮件数据，禁止直接打印。
            code = getattr(error, 'code', None)
            detail = '认证失败，请检查访问令牌' if code in (401, 403) else '远程 MCP 请求失败；写操作请先查询状态，不要盲目重试'
            print(detail, file=sys.stderr)
            if 'id' in message:
                write_message({'jsonrpc': '2.0', 'id': message['id'],
                               'error': {'code': -32000, 'message': detail}})


if __name__ == '__main__':
    sys.exit(main())
