from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
import http.client, urllib.request, json, time
class Proxy(BaseHTTPRequestHandler):
 def forward(self):
  with urllib.request.urlopen('http://localhost:34444/fixture/mode') as r: mode=json.load(r)['mode']
  if '/metadata/' in self.path and mode == 'slow': time.sleep(.5)
  if '/metadata/' in self.path and mode == 'failed':
   body=b'{"errors":["fixture permission denied"]}'
   self.send_response(403); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body); return
  c=http.client.HTTPConnection('127.0.0.1',34200)
  body=self.rfile.read(int(self.headers.get('Content-Length',0)))
  c.request(self.command,self.path,body,dict(self.headers))
  r=c.getresponse(); data=r.read()
  self.send_response(r.status)
  self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(data))); self.end_headers(); self.wfile.write(data); c.close()
 do_GET=do_POST=do_PUT=do_DELETE=do_LIST=forward
 def log_message(self,*args): pass
ThreadingHTTPServer(('127.0.0.1',34445),Proxy).serve_forever()
