from http.server import BaseHTTPRequestHandler, HTTPServer
import random

# Rotating index for status codes
status_codes = [200, 403, 503, 1200] 
index = 0
counter = 1

class RotatingStatusHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        global index
        global counter
        # Select the current status code
        status_code = status_codes[index]
        index = (index + 1) % len(status_codes)  # Rotate the index
        
        # Send response header with the selected status code
        self.send_response(status_code)
        self.send_header("Content-type", "text/html")
        if status_code == 200:
            self.send_header("x-since-time", str(counter))
            counter += 1
        self.end_headers()
        
        # Send a response body
        self.wfile.write(f"Status code: {status_code}".encode("utf-8"))

def run(server_class=HTTPServer, handler_class=RotatingStatusHandler, port=8080):
    server_address = ('', port)
    httpd = server_class(server_address, handler_class)
    print(f"Starting server on port {port}...")
    httpd.serve_forever()

if __name__ == "__main__":
    run()
