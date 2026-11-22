# A minimal Subsonic server for the installer test: answers ping (any user and password).
import http.server, json
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        body = json.dumps({'subsonic-response': {'status': 'ok', 'version': '1.16.1'}}).encode()
        self.send_response(200); self.send_header('content-type', 'application/json'); self.end_headers(); self.wfile.write(body)
http.server.ThreadingHTTPServer(('127.0.0.1', 4533), H).serve_forever()
