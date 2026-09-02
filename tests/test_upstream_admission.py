import threading
import time
import unittest

from easy_multi_provider.upstream_admission import (
    UpstreamAdmissionController,
    UpstreamAdmissionError,
)


class UpstreamAdmissionTests(unittest.TestCase):
    def test_same_identity_waits_until_active_generation_releases(self):
        controller = UpstreamAdmissionController(
            per_identity_limit=1, queue_timeout=1
        )
        first = controller.acquire("account-a")
        started = threading.Event()
        acquired = threading.Event()
        snapshots = []

        def waiter():
            started.set()
            with controller.acquire("account-a") as lease:
                snapshots.append(lease.snapshot)
                acquired.set()

        thread = threading.Thread(target=waiter)
        thread.start()
        self.assertTrue(started.wait(1))
        time.sleep(0.02)
        self.assertFalse(acquired.is_set())

        first.release()
        self.assertTrue(acquired.wait(1))
        thread.join(1)

        self.assertFalse(thread.is_alive())
        self.assertEqual(snapshots[0].active, 1)
        self.assertGreaterEqual(snapshots[0].wait_ms, 1)

    def test_different_identities_do_not_block_each_other(self):
        controller = UpstreamAdmissionController(per_identity_limit=1)
        first = controller.acquire("account-a")
        second = controller.acquire("account-b", timeout=0)
        try:
            self.assertEqual(first.snapshot.active, 1)
            self.assertEqual(second.snapshot.active, 1)
        finally:
            first.release()
            second.release()

    def test_queue_timeout_is_actionable_and_does_not_leak_capacity(self):
        controller = UpstreamAdmissionController(per_identity_limit=1)
        first = controller.acquire("account-a")
        with self.assertRaises(UpstreamAdmissionError) as raised:
            controller.acquire("account-a", timeout=0)
        self.assertEqual(raised.exception.status, 503)
        self.assertEqual(raised.exception.error_class, "upstream_capacity")
        self.assertEqual(
            raised.exception.failure_reason, "concurrency_queue_timeout"
        )

        first.release()
        replacement = controller.acquire("account-a", timeout=0)
        replacement.release()

    def test_release_is_idempotent(self):
        controller = UpstreamAdmissionController(per_identity_limit=1)
        lease = controller.acquire("account-a")
        lease.release()
        lease.release()
        replacement = controller.acquire("account-a", timeout=0)
        replacement.release()


if __name__ == "__main__":
    unittest.main()
