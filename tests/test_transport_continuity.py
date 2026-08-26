import unittest

from easy_multi_provider.transport_continuity import (
    PREVIOUS_RESPONSE_NOT_FOUND_CODE,
    PREVIOUS_RESPONSE_NOT_FOUND_MESSAGE,
    TransportContinuityAdapter,
    TransportContinuityDecision,
    TransportContinuityState,
)


class TransportContinuityTests(unittest.TestCase):
    def test_same_live_route_preserves_incremental_continuity(self):
        decision = TransportContinuityAdapter().decide(
            {"previous_response_id": "resp_live", "input": []},
            TransportContinuityState(
                current_route_identity="route-a",
                live_route_identity="route-a",
                previous_response_id="resp_live",
                live_previous_response_id="resp_live",
                upstream_incremental_capable=True,
                live_connection=True,
            ),
        )

        self.assertEqual(
            decision, TransportContinuityDecision.CONTINUE_INCREMENTAL
        )

    def test_unavailable_previous_response_requests_full_retry(self):
        decision = TransportContinuityAdapter().decide(
            {"previous_response_id": "resp_lost", "input": []},
            TransportContinuityState(
                current_route_identity="route-b",
                live_route_identity="route-a",
                previous_response_id="resp_lost",
                live_previous_response_id="resp_lost",
                upstream_incremental_capable=True,
                live_connection=True,
            ),
        )

        self.assertEqual(
            decision, TransportContinuityDecision.PREVIOUS_RESPONSE_NOT_FOUND
        )
        self.assertEqual(PREVIOUS_RESPONSE_NOT_FOUND_CODE, "previous_response_not_found")
        self.assertEqual(
            PREVIOUS_RESPONSE_NOT_FOUND_MESSAGE,
            "Previous response was not found. Retrying the full request.",
        )

    def test_request_without_previous_response_is_full(self):
        decision = TransportContinuityAdapter().decide(
            {"input": []},
            TransportContinuityState(
                current_route_identity=None,
                live_route_identity=None,
                previous_response_id=None,
                live_previous_response_id=None,
                upstream_incremental_capable=False,
                live_connection=False,
            ),
        )

        self.assertEqual(decision, TransportContinuityDecision.FULL_REQUEST)


if __name__ == "__main__":
    unittest.main()
