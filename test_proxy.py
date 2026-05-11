#!/usr/bin/env python3
"""测试 model-proxy 的 OpenAI 上游转换、流式转发和认证"""

import json
import os
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
RELEASE_BINARY = os.path.join(BASE_DIR, "target", "release", "model-proxy")


def find_free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_for_port(port, timeout=5.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError(f"代理未在 {timeout}s 内监听端口 {port}")


def build_release_binary():
    subprocess.run(["cargo", "build", "--release"], cwd=BASE_DIR, check=True)


def write_test_config(temp_dir, proxy_port, provider_port):
    config = f"""
[server]
port = {proxy_port}
api_key = "test-proxy-key"
max_body_bytes = 67108864

[provider]
base_url = "http://127.0.0.1:{provider_port}"
api_key = "test-provider-key"
model = "deepseek-v4-pro"
format = "openai"
"""
    with open(os.path.join(temp_dir, "config.toml"), "w", encoding="utf-8") as f:
        f.write(config)


def start_mock_provider(port):
    """启动模拟的 OpenAI 兼容 Provider"""
    import http.server
    import socketserver

    class ReusableTCPServer(socketserver.TCPServer):
        allow_reuse_address = True

    class MockHandler(http.server.BaseHTTPRequestHandler):
        def log_message(self, format, *args):
            pass

        def do_POST(self):
            try:
                self.handle_post()
            except AssertionError as e:
                self.send_response(500)
                self.send_header("Content-Type", "text/plain")
                self.end_headers()
                self.wfile.write(str(e).encode())

        def handle_post(self):
            assert self.path == "/v1/chat/completions", f"路径错误: {self.path}"
            content_len = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(content_len)
            req = json.loads(body)

            assert req.get("model") == "deepseek-v4-pro", f"模型名称未替换: {req.get('model')}"
            assert self.headers.get("authorization") == "Bearer test-provider-key"
            assert req["messages"][0]["role"] == "user"
            assert req["messages"][0]["content"] == "Hi"

            if req.get("stream"):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()

                chunks = [
                    {
                        "id": "chatcmpl_01",
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"role": "assistant"},
                                "finish_reason": None,
                            }
                        ],
                    },
                    {
                        "id": "chatcmpl_01",
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"content": "Hello"},
                                "finish_reason": None,
                            }
                        ],
                    },
                    {
                        "id": "chatcmpl_01",
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"content": " World"},
                                "finish_reason": None,
                            }
                        ],
                    },
                    {
                        "id": "chatcmpl_01",
                        "choices": [
                            {
                                "index": 0,
                                "delta": {},
                                "finish_reason": "stop",
                            }
                        ],
                    },
                    {
                        "id": "chatcmpl_01",
                        "choices": [],
                        "usage": {"prompt_tokens": 4, "completion_tokens": 2},
                    },
                ]
                for chunk in chunks:
                    self.wfile.write(f"data: {json.dumps(chunk)}\r\n\r\n".encode())
                    self.wfile.flush()
                    time.sleep(0.02)
                self.wfile.write(b"data: [DONE]\r\n\r\n")
                self.wfile.flush()
                return

            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            resp = {
                "id": "chatcmpl_01",
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "content": "Hello World",
                        },
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2},
            }
            self.wfile.write(json.dumps(resp).encode())

    server = ReusableTCPServer(("127.0.0.1", port), MockHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def start_proxy(temp_dir, proxy_port, provider_port):
    write_test_config(temp_dir, proxy_port, provider_port)
    proxy_binary = os.path.join(temp_dir, "model-proxy")
    shutil.copy2(RELEASE_BINARY, proxy_binary)
    proc = subprocess.Popen(
        [proxy_binary],
        cwd=temp_dir,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    wait_for_port(proxy_port)
    return proc


def test_non_stream(proxy_port):
    req = urllib.request.Request(
        f"http://127.0.0.1:{proxy_port}/v1/messages",
        data=json.dumps(
            {
                "model": "user-custom-model",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "Hi"}],
                "stream": False,
            }
        ).encode(),
        headers={
            "Content-Type": "application/json",
            "x-api-key": "test-proxy-key",
        },
    )
    resp = urllib.request.urlopen(req)
    data = json.loads(resp.read())
    assert data["content"][0]["text"] == "Hello World", f"非流式响应异常: {data}"
    assert data["usage"]["input_tokens"] == 4
    assert data["usage"]["output_tokens"] == 2
    print("✓ 非流式测试通过")


def test_stream(proxy_port):
    req = urllib.request.Request(
        f"http://127.0.0.1:{proxy_port}/v1/messages",
        data=json.dumps(
            {
                "model": "user-custom-model",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "Hi"}],
                "stream": True,
            }
        ).encode(),
        headers={
            "Content-Type": "application/json",
            "x-api-key": "test-proxy-key",
        },
    )
    resp = urllib.request.urlopen(req)
    text = resp.read().decode()
    assert "Hello" in text, f"流式响应缺少内容: {text}"
    assert "World" in text, f"流式响应缺少内容: {text}"
    assert "event: message_start" in text, f"流式响应格式错误: {text}"
    assert "event: message_stop" in text, f"流式响应格式错误: {text}"
    print("✓ 流式测试通过")


def test_auth_fail(proxy_port):
    req = urllib.request.Request(
        f"http://127.0.0.1:{proxy_port}/v1/messages",
        data=json.dumps({"model": "x", "max_tokens": 10, "messages": []}).encode(),
        headers={"Content-Type": "application/json", "x-api-key": "wrong-key"},
    )
    try:
        urllib.request.urlopen(req)
        assert False, "应该返回 401"
    except urllib.error.HTTPError as e:
        assert e.code == 401, f"应该返回 401，实际 {e.code}"
        print("✓ 认证失败测试通过")


def main():
    build_release_binary()
    proxy_port = find_free_port()
    provider_port = find_free_port()
    server = start_mock_provider(provider_port)

    with tempfile.TemporaryDirectory(prefix="model-proxy-test-") as temp_dir:
        proxy_proc = start_proxy(temp_dir, proxy_port, provider_port)
        try:
            test_non_stream(proxy_port)
            test_stream(proxy_port)
            test_auth_fail(proxy_port)
            print("\n🎉 所有测试通过！")
        finally:
            proxy_proc.terminate()
            try:
                proxy_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proxy_proc.kill()
                proxy_proc.wait(timeout=5)
            server.shutdown()
            server.server_close()


if __name__ == "__main__":
    main()
