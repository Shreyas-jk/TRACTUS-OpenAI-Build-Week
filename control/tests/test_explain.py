import pytest

from control.explain import explain_divergence


class RaisingResponses:
    async def create(self, **kwargs):
        raise RuntimeError("API unavailable")


class RaisingClient:
    responses = RaisingResponses()


@pytest.mark.asyncio
async def test_explainer_falls_back_after_api_error() -> None:
    result = await explain_divergence(
        "Fix a test",
        "cargo add axios",
        [{"clause": "deps_may_change = false"}],
        client=RaisingClient(),
    )

    assert result == "Blocked: deps_may_change = false."
