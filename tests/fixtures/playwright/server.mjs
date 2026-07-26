import http from 'node:http';
import fs from 'node:fs';

const port = Number(process.env.BENDER_FIXTURE_PORT || 41739);
const server = http.createServer((request, response) => {
  if (request.url === '/health') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end('{"ok":true}');
    return;
  }
  const fixed = fs.existsSync(new URL('fixed.txt', import.meta.url));
  response.writeHead(200, { 'content-type': 'text/html' });
  response.end(`<!doctype html>
    <button id="action">Run action</button>
    <output id="result">waiting</output>
    <script>
      document.querySelector('#action').addEventListener('click', () => {
        if (${fixed}) {
          document.querySelector('#result').textContent = 'complete';
        } else {
          console.error('deliberate first-attempt UI bug');
        }
      });
    </script>`);
});
server.listen(port, '127.0.0.1');
