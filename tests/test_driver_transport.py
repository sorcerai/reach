import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import MagicMock

from scripts.reach_drive import ReachDriver


class DriverTransportTests(unittest.TestCase):
    def test_metadata_redirect_cannot_forward_a_lease_capability(self):
        reached_destination = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                if self.path == "/agent/screens":
                    self.send_response(302)
                    self.send_header("Location", "/redirect-target")
                    self.end_headers()
                    return
                reached_destination.append(self.headers.get("X-Lease-Token"))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'[]')

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        driver = ReachDriver(
            api_url=f"http://127.0.0.1:{server.server_port}",
            lease_token="fixture-capability", handoff_gen=1, enable_audit=False,
        )
        try:
            self.assertEqual(driver.get_screens(), [])
            self.assertEqual(reached_destination, [])
        finally:
            driver.cleanup()
            server.shutdown()
            server.server_close()
            thread.join()
    def test_lease_transport_failure_is_uncertain_without_retry(self):
        driver = ReachDriver(
            api_url="http://127.0.0.1:4200",
            screen=0,
            enable_audit=False,
        )
        opener = MagicMock()
        opener.open.side_effect = TimeoutError("connection timed out")
        driver._api_opener = opener

        try:
            with self.assertRaisesRegex(RuntimeError, "uncertain"):
                driver.lease_screen(owner="ReachBot")
            self.assertEqual(driver.last_lease_cleanup, {"status": "uncertain"})
            self.assertIsNone(driver.lease_token)
            self.assertIsNone(driver.handoff_gen)
            self.assertEqual(opener.open.call_count, 1)
        finally:
            driver.cleanup()
